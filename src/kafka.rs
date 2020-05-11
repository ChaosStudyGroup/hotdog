/**
 * The Kafka module contains all the tooling/code necessary for connecting hotdog to Kafka for
 * sending log lines along as Kafka messages
 */
use async_std::sync::Arc;
use crossbeam::channel::{bounded, Receiver, Sender};
use dipstick::*;
use futures::executor::ThreadPool;
use futures::*;
use log::*;
use rdkafka::client::DefaultClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::error::{KafkaError, RDKafkaError};
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::collections::HashMap;
use std::time::Duration;

/**
 * KafkaMessage just carries a message and its destination topic between tasks
 */
pub struct KafkaMessage {
    topic: String,
    msg: String,
}

impl KafkaMessage {
    pub fn new(topic: String, msg: String) -> KafkaMessage {
        KafkaMessage { topic, msg }
    }
}

/**
 * The Kafka struct acts as the primary interface between hotdog and Kafka
 */
pub struct Kafka {
    /*
     * I'm not super thrilled about wrapping the FutureProducer in an option, but it's the only way
     * that I can think to create an effective two-phase construction of this struct between
     * ::new() and the .connect() function
     */
    producer: Option<FutureProducer<DefaultClientContext>>,
    metrics: Option<Arc<LockingOutput>>,
    rx: Receiver<KafkaMessage>,
    tx: Sender<KafkaMessage>,
}

impl Kafka {
    pub fn new(message_max: usize) -> Kafka {
        let (tx, rx) = bounded(message_max);
        Kafka {
            metrics: None,
            producer: None,
            tx,
            rx,
        }
    }

    pub fn with_metrics(&mut self, metrics: Arc<LockingOutput>) {
        self.metrics = Some(metrics);
    }

    /**
     * connect() will inherently validate the configuration and perform a blocking call to the
     * configured bootstrap.servers in order to determine whether Kafka is reachable.
     *
     * Assuming Kafka can be reached, the connect() call will construct the producer and return
     * true
     *
     * If timeout_ms is not specified, a default 10s timeout will be used
     */
    pub fn connect(
        &mut self,
        rdkafka_conf: &HashMap<String, String>,
        timeout_ms: Option<Duration>,
    ) -> bool {
        let mut rd_conf = ClientConfig::new();

        for (key, value) in rdkafka_conf.iter() {
            rd_conf.set(key, value);
        }

        /*
         * Allow our brokers to be defined at runtime overriding the configuration
         */
        if let Ok(broker) = std::env::var("KAFKA_BROKER") {
            rd_conf.set("bootstrap.servers", &broker);
        }

        let consumer: BaseConsumer = rd_conf
            .create()
            .expect("Creation of Kafka consumer (for metadata) failed");

        let timeout = match timeout_ms {
            Some(ms) => ms,
            None => Duration::from_secs(10),
        };

        if let Ok(metadata) = consumer.fetch_metadata(None, timeout) {
            debug!("  Broker count: {}", metadata.brokers().len());
            debug!("  Topics count: {}", metadata.topics().len());
            debug!("  Metadata broker name: {}", metadata.orig_broker_name());
            debug!("  Metadata broker id: {}\n", metadata.orig_broker_id());

            self.producer = Some(
                rd_conf
                    .create()
                    .expect("Failed to create the Kafka producer!"),
            );

            return true;
        }

        warn!("Failed to connect to a Kafka broker");

        return false;
    }

    /**
     * get_sender() will return a cloned reference to the sender suitable for tasks or threads to
     * consume and take ownership of
     */
    pub fn get_sender(&self) -> Sender<KafkaMessage> {
        self.tx.clone()
    }

    /**
     * sendloop should be called in a thread/task and will never return
     */
    pub fn sendloop(&self) -> ! {
        if self.producer.is_none() {
            panic!("Cannot enter the sendloop() without a valid producer");
        }

        let pool = ThreadPool::new().unwrap();
        let producer = self.producer.as_ref().unwrap();

        // How long should we wait for an internal message to show up on our channel
        let timeout_ms = Duration::from_millis(100);

        // TODO: replace me with a select
        loop {
            if let Ok(kmsg) = self.rx.recv_timeout(timeout_ms) {
                /* Note, setting the `K` (key) type on FutureRecord to a string
                 * even though we're explicitly not sending a key
                 */
                let record = FutureRecord::<String, String>::to(&kmsg.topic).payload(&kmsg.msg);

                /*
                 * Intentionally setting the timeout_ms to -1 here so this blocks forever if the
                 * outbound librdkafka queue is full. This will block up the crossbeam channel
                 * properly and cause messages to begin to be dropped, rather than buffering
                 * "forever" inside of hotdog
                 */
                if let Some(metrics) = &self.metrics {
                    let m = metrics.clone();
                    let timer = metrics.timer("kafka.producer.sent");
                    let handle = timer.start();
                    let fut = producer
                        .send(record, -1 as i64)
                        .then(move |res| {
                            // unwrap the always-Ok resultto get the real DeliveryFuture result
                            let delivery_result = res.unwrap();

                            match delivery_result {
                                Ok(_) => {
                                    timer.stop(handle);
                                    m.counter("kafka.submitted").count(1);
                                }
                                Err((err, msg)) => {
                                    match err {
                                        /*
                                         * err_type will be one of RdKafkaError types defined:
                                         * https://docs.rs/rdkafka/0.23.1/rdkafka/error/enum.RDKafkaError.html
                                         */
                                        KafkaError::MessageProduction(err_type) => {
                                            error!(
                                                "Failed to send message to Kafka due to: {}",
                                                err_type
                                            );
                                            m.counter(&format!(
                                                "kafka.producer.error.{}",
                                                metric_name_for(err_type)
                                            ))
                                            .count(1);
                                        }
                                        _ => {
                                            error!("Failed to send message to Kafka!");
                                            m.counter("kafka.producer.error.generic").count(1);
                                        }
                                    }
                                }
                            }
                            future::ok::<bool, bool>(true)
                        })
                        /* Need to obliterate the Output type defined on then's TryFuture with
                         * a map so this can be spawned off to the threadpool, which requires
                         * Future<Output = ()>
                         */
                        .map(|_| ());
                    /*
                     * Resolve this future off in the threadpool so we can report metrics once
                     * things are complete
                     */
                    pool.spawn_ok(fut);
                } else {
                    let _future = producer.send(record, -1 as i64);
                }
            }
        }
    }
}

/**
 * A simple function for formatting the generated strings from RDKafkaError to be useful as metric
 * names for systems like statsd
 */
fn metric_name_for(err: RDKafkaError) -> String {
    if let Some(name) = err.to_string().to_lowercase().split(' ').next() {
        return name.to_string();
    }
    return String::from("unknown");
}

#[cfg(test)]
mod tests {
    use super::*;

    /**
     * Test that trying to connect to a nonexistent cluster returns false
     */
    #[test]
    fn test_connect_bad_cluster() {
        let mut conf = HashMap::<String, String>::new();
        conf.insert(
            String::from("bootstrap.servers"),
            String::from("example.com:9092"),
        );

        let mut k = Kafka::new(0);
        assert_eq!(false, k.connect(&conf, Some(Duration::from_secs(1))));
    }

    /**
     * Tests for converting RDKafkaError strings into statsd suitable metric strings
     */
    #[test]
    fn test_metric_name_1() {
        assert_eq!(
            "messagetimedout",
            metric_name_for(RDKafkaError::MessageTimedOut)
        );
    }
    #[test]
    fn test_metric_name_2() {
        assert_eq!("unknowntopic", metric_name_for(RDKafkaError::UnknownTopic));
    }
    #[test]
    fn test_metric_name_3() {
        assert_eq!("readonly", metric_name_for(RDKafkaError::ReadOnly));
    }
}
