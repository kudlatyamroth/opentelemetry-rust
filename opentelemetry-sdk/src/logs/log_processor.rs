use crate::{
    export::logs::{ExportResult, LogBatch, LogExporter},
    logs::{LogError, LogRecord, LogResult},
    runtime::{RuntimeChannel, TrySend},
    Resource,
};
use futures_channel::oneshot;
use futures_util::{
    future::{self, Either},
    {pin_mut, stream, StreamExt as _},
};
#[cfg(feature = "spec_unstable_logs_enabled")]
use opentelemetry::logs::Severity;
use opentelemetry::{otel_debug, otel_error, otel_warn, InstrumentationScope};

use std::sync::atomic::AtomicBool;
use std::{cmp::min, env, sync::Mutex};
use std::{
    fmt::{self, Debug, Formatter},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

/// Delay interval between two consecutive exports.
const OTEL_BLRP_SCHEDULE_DELAY: &str = "OTEL_BLRP_SCHEDULE_DELAY";
/// Default delay interval between two consecutive exports.
const OTEL_BLRP_SCHEDULE_DELAY_DEFAULT: u64 = 1_000;
/// Maximum allowed time to export data.
const OTEL_BLRP_EXPORT_TIMEOUT: &str = "OTEL_BLRP_EXPORT_TIMEOUT";
/// Default maximum allowed time to export data.
const OTEL_BLRP_EXPORT_TIMEOUT_DEFAULT: u64 = 30_000;
/// Maximum queue size.
const OTEL_BLRP_MAX_QUEUE_SIZE: &str = "OTEL_BLRP_MAX_QUEUE_SIZE";
/// Default maximum queue size.
const OTEL_BLRP_MAX_QUEUE_SIZE_DEFAULT: usize = 2_048;
/// Maximum batch size, must be less than or equal to OTEL_BLRP_MAX_QUEUE_SIZE.
const OTEL_BLRP_MAX_EXPORT_BATCH_SIZE: &str = "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE";
/// Default maximum batch size.
const OTEL_BLRP_MAX_EXPORT_BATCH_SIZE_DEFAULT: usize = 512;

/// The interface for plugging into a [`Logger`].
///
/// [`Logger`]: crate::logs::Logger
pub trait LogProcessor: Send + Sync + Debug {
    /// Called when a log record is ready to processed and exported.
    ///
    /// This method receives a mutable reference to `LogData`. If the processor
    /// needs to handle the export asynchronously, it should clone the data to
    /// ensure it can be safely processed without lifetime issues. Any changes
    /// made to the log data in this method will be reflected in the next log
    /// processor in the chain.
    ///
    /// # Parameters
    /// - `record`: A mutable reference to `LogData` representing the log record.
    /// - `instrumentation`: The instrumentation scope associated with the log record.
    fn emit(&self, data: &mut LogRecord, instrumentation: &InstrumentationScope);
    /// Force the logs lying in the cache to be exported.
    fn force_flush(&self) -> LogResult<()>;
    /// Shuts down the processor.
    /// After shutdown returns the log processor should stop processing any logs.
    /// It's up to the implementation on when to drop the LogProcessor.
    fn shutdown(&self) -> LogResult<()>;
    #[cfg(feature = "spec_unstable_logs_enabled")]
    /// Check if logging is enabled
    fn event_enabled(&self, _level: Severity, _target: &str, _name: &str) -> bool {
        // By default, all logs are enabled
        true
    }

    /// Set the resource for the log processor.
    fn set_resource(&self, _resource: &Resource) {}
}

/// A [LogProcessor] that passes logs to the configured `LogExporter`, as soon
/// as they are emitted, without any batching. This is typically useful for
/// debugging and testing. For scenarios requiring higher
/// performance/throughput, consider using [BatchLogProcessor].
#[derive(Debug)]
pub struct SimpleLogProcessor {
    exporter: Mutex<Box<dyn LogExporter>>,
    is_shutdown: AtomicBool,
}

impl SimpleLogProcessor {
    pub(crate) fn new(exporter: Box<dyn LogExporter>) -> Self {
        SimpleLogProcessor {
            exporter: Mutex::new(exporter),
            is_shutdown: AtomicBool::new(false),
        }
    }
}

impl LogProcessor for SimpleLogProcessor {
    fn emit(&self, record: &mut LogRecord, instrumentation: &InstrumentationScope) {
        // noop after shutdown
        if self.is_shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            // this is a warning, as the user is trying to log after the processor has been shutdown
            otel_warn!(
                name: "SimpleLogProcessor.Emit.ProcessorShutdown",
            );
            return;
        }

        let result = self
            .exporter
            .lock()
            .map_err(|_| LogError::MutexPoisoned("SimpleLogProcessor".into()))
            .and_then(|mut exporter| {
                let log_tuple = &[(record as &LogRecord, instrumentation)];
                futures_executor::block_on(exporter.export(LogBatch::new(log_tuple)))
            });
        // Handle errors with specific static names
        match result {
            Err(LogError::MutexPoisoned(_)) => {
                // logging as debug as this is not a user error
                otel_debug!(
                    name: "SimpleLogProcessor.Emit.MutexPoisoning",
                );
            }
            Err(err) => {
                otel_error!(
                    name: "SimpleLogProcessor.Emit.ExportError",
                    error = format!("{}",err)
                );
            }
            _ => {}
        }
    }

    fn force_flush(&self) -> LogResult<()> {
        Ok(())
    }

    fn shutdown(&self) -> LogResult<()> {
        self.is_shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut exporter) = self.exporter.lock() {
            exporter.shutdown();
            Ok(())
        } else {
            Err(LogError::MutexPoisoned("SimpleLogProcessor".into()))
        }
    }

    fn set_resource(&self, resource: &Resource) {
        if let Ok(mut exporter) = self.exporter.lock() {
            exporter.set_resource(resource);
        }
    }
}

/// A [`LogProcessor`] that asynchronously buffers log records and reports
/// them at a pre-configured interval.
pub struct BatchLogProcessor<R: RuntimeChannel> {
    message_sender: R::Sender<BatchMessage>,
}

impl<R: RuntimeChannel> Debug for BatchLogProcessor<R> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchLogProcessor")
            .field("message_sender", &self.message_sender)
            .finish()
    }
}

impl<R: RuntimeChannel> LogProcessor for BatchLogProcessor<R> {
    fn emit(&self, record: &mut LogRecord, instrumentation: &InstrumentationScope) {
        let result = self.message_sender.try_send(BatchMessage::ExportLog((
            record.clone(),
            instrumentation.clone(),
        )));

        // TODO - Implement throttling to prevent error flooding when the queue is full or closed.
        if let Err(err) = result {
            otel_error!(
                name: "BatchLogProcessor.Export.Error",
                error = format!("{}", err)
            );
        }
    }

    fn force_flush(&self) -> LogResult<()> {
        let (res_sender, res_receiver) = oneshot::channel();
        self.message_sender
            .try_send(BatchMessage::Flush(Some(res_sender)))
            .map_err(|err| LogError::Other(err.into()))?;

        futures_executor::block_on(res_receiver)
            .map_err(|err| LogError::Other(err.into()))
            .and_then(std::convert::identity)
    }

    fn shutdown(&self) -> LogResult<()> {
        let (res_sender, res_receiver) = oneshot::channel();
        self.message_sender
            .try_send(BatchMessage::Shutdown(res_sender))
            .map_err(|err| LogError::Other(err.into()))?;

        futures_executor::block_on(res_receiver)
            .map_err(|err| LogError::Other(err.into()))
            .and_then(std::convert::identity)
    }

    fn set_resource(&self, resource: &Resource) {
        let resource = Arc::new(resource.clone());
        let _ = self
            .message_sender
            .try_send(BatchMessage::SetResource(resource));
    }
}

impl<R: RuntimeChannel> BatchLogProcessor<R> {
    pub(crate) fn new(mut exporter: Box<dyn LogExporter>, config: BatchConfig, runtime: R) -> Self {
        let (message_sender, message_receiver) =
            runtime.batch_message_channel(config.max_queue_size);
        let inner_runtime = runtime.clone();

        // Spawn worker process via user-defined spawn function.
        runtime.spawn(Box::pin(async move {
            // Timer will take a reference to the current runtime, so its important we do this within the
            // runtime.spawn()
            let ticker = inner_runtime
                .interval(config.scheduled_delay)
                .skip(1) // The ticker is fired immediately, so we should skip the first one to align with the interval.
                .map(|_| BatchMessage::Flush(None));
            let timeout_runtime = inner_runtime.clone();
            let mut logs = Vec::new();
            let mut messages = Box::pin(stream::select(message_receiver, ticker));

            while let Some(message) = messages.next().await {
                match message {
                    // Log has finished, add to buffer of pending logs.
                    BatchMessage::ExportLog(log) => {
                        logs.push(log);
                        if logs.len() == config.max_export_batch_size {
                            let result = export_with_timeout(
                                config.max_export_timeout,
                                exporter.as_mut(),
                                &timeout_runtime,
                                logs.split_off(0),
                            )
                            .await;

                            if let Err(err) = result {
                                otel_error!(
                                    name: "BatchLogProcessor.Export.Error",
                                    error = format!("{}", err)
                                );
                            }
                        }
                    }
                    // Log batch interval time reached or a force flush has been invoked, export current spans.
                    BatchMessage::Flush(res_channel) => {
                        let result = export_with_timeout(
                            config.max_export_timeout,
                            exporter.as_mut(),
                            &timeout_runtime,
                            logs.split_off(0),
                        )
                        .await;

                        if let Some(channel) = res_channel {
                            if let Err(send_error) = channel.send(result) {
                                otel_debug!(
                                    name: "BatchLogProcessor.Flush.SendResultError",
                                    error = format!("{:?}", send_error),
                                );
                            }
                        }
                    }
                    // Stream has terminated or processor is shutdown, return to finish execution.
                    BatchMessage::Shutdown(ch) => {
                        let result = export_with_timeout(
                            config.max_export_timeout,
                            exporter.as_mut(),
                            &timeout_runtime,
                            logs.split_off(0),
                        )
                        .await;

                        exporter.shutdown();

                        if let Err(send_error) = ch.send(result) {
                            otel_debug!(
                                name: "BatchLogProcessor.Shutdown.SendResultError",
                                error = format!("{:?}", send_error),
                            );
                        }
                        break;
                    }
                    // propagate the resource
                    BatchMessage::SetResource(resource) => {
                        exporter.set_resource(&resource);
                    }
                }
            }
        }));
        // Return batch processor with link to worker
        BatchLogProcessor { message_sender }
    }

    /// Create a new batch processor builder
    pub fn builder<E>(exporter: E, runtime: R) -> BatchLogProcessorBuilder<E, R>
    where
        E: LogExporter,
    {
        BatchLogProcessorBuilder {
            exporter,
            config: Default::default(),
            runtime,
        }
    }
}

async fn export_with_timeout<R, E>(
    time_out: Duration,
    exporter: &mut E,
    runtime: &R,
    batch: Vec<(LogRecord, InstrumentationScope)>,
) -> ExportResult
where
    R: RuntimeChannel,
    E: LogExporter + ?Sized,
{
    if batch.is_empty() {
        return Ok(());
    }

    // TBD - Can we avoid this conversion as it involves heap allocation with new vector?
    let log_vec: Vec<(&LogRecord, &InstrumentationScope)> = batch
        .iter()
        .map(|log_data| (&log_data.0, &log_data.1))
        .collect();
    let export = exporter.export(LogBatch::new(log_vec.as_slice()));
    let timeout = runtime.delay(time_out);
    pin_mut!(export);
    pin_mut!(timeout);
    match future::select(export, timeout).await {
        Either::Left((export_res, _)) => export_res,
        Either::Right((_, _)) => ExportResult::Err(LogError::ExportTimedOut(time_out)),
    }
}

/// Batch log processor configuration.
/// Use [`BatchConfigBuilder`] to configure your own instance of [`BatchConfig`].
#[derive(Debug)]
pub struct BatchConfig {
    /// The maximum queue size to buffer logs for delayed processing. If the
    /// queue gets full it drops the logs. The default value of is 2048.
    max_queue_size: usize,

    /// The delay interval in milliseconds between two consecutive processing
    /// of batches. The default value is 1 second.
    scheduled_delay: Duration,

    /// The maximum number of logs to process in a single batch. If there are
    /// more than one batch worth of logs then it processes multiple batches
    /// of logs one batch after the other without any delay. The default value
    /// is 512.
    max_export_batch_size: usize,

    /// The maximum duration to export a batch of data.
    max_export_timeout: Duration,
}

impl Default for BatchConfig {
    fn default() -> Self {
        BatchConfigBuilder::default().build()
    }
}

/// A builder for creating [`BatchConfig`] instances.
#[derive(Debug)]
pub struct BatchConfigBuilder {
    max_queue_size: usize,
    scheduled_delay: Duration,
    max_export_batch_size: usize,
    max_export_timeout: Duration,
}

impl Default for BatchConfigBuilder {
    /// Create a new [`BatchConfigBuilder`] initialized with default batch config values as per the specs.
    /// The values are overriden by environment variables if set.
    /// The supported environment variables are:
    /// * `OTEL_BLRP_MAX_QUEUE_SIZE`
    /// * `OTEL_BLRP_SCHEDULE_DELAY`
    /// * `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE`
    /// * `OTEL_BLRP_EXPORT_TIMEOUT`
    fn default() -> Self {
        BatchConfigBuilder {
            max_queue_size: OTEL_BLRP_MAX_QUEUE_SIZE_DEFAULT,
            scheduled_delay: Duration::from_millis(OTEL_BLRP_SCHEDULE_DELAY_DEFAULT),
            max_export_batch_size: OTEL_BLRP_MAX_EXPORT_BATCH_SIZE_DEFAULT,
            max_export_timeout: Duration::from_millis(OTEL_BLRP_EXPORT_TIMEOUT_DEFAULT),
        }
        .init_from_env_vars()
    }
}

impl BatchConfigBuilder {
    /// Set max_queue_size for [`BatchConfigBuilder`].
    /// It's the maximum queue size to buffer logs for delayed processing.
    /// If the queue gets full it will drop the logs.
    /// The default value of is 2048.
    pub fn with_max_queue_size(mut self, max_queue_size: usize) -> Self {
        self.max_queue_size = max_queue_size;
        self
    }

    /// Set scheduled_delay for [`BatchConfigBuilder`].
    /// It's the delay interval in milliseconds between two consecutive processing of batches.
    /// The default value is 1000 milliseconds.
    pub fn with_scheduled_delay(mut self, scheduled_delay: Duration) -> Self {
        self.scheduled_delay = scheduled_delay;
        self
    }

    /// Set max_export_timeout for [`BatchConfigBuilder`].
    /// It's the maximum duration to export a batch of data.
    /// The default value is 30000 milliseconds.
    pub fn with_max_export_timeout(mut self, max_export_timeout: Duration) -> Self {
        self.max_export_timeout = max_export_timeout;
        self
    }

    /// Set max_export_batch_size for [`BatchConfigBuilder`].
    /// It's the maximum number of logs to process in a single batch. If there are
    /// more than one batch worth of logs then it processes multiple batches
    /// of logs one batch after the other without any delay.
    /// The default value is 512.
    pub fn with_max_export_batch_size(mut self, max_export_batch_size: usize) -> Self {
        self.max_export_batch_size = max_export_batch_size;
        self
    }

    /// Builds a `BatchConfig` enforcing the following invariants:
    /// * `max_export_batch_size` must be less than or equal to `max_queue_size`.
    pub fn build(self) -> BatchConfig {
        // max export batch size must be less or equal to max queue size.
        // we set max export batch size to max queue size if it's larger than max queue size.
        let max_export_batch_size = min(self.max_export_batch_size, self.max_queue_size);

        BatchConfig {
            max_queue_size: self.max_queue_size,
            scheduled_delay: self.scheduled_delay,
            max_export_timeout: self.max_export_timeout,
            max_export_batch_size,
        }
    }

    fn init_from_env_vars(mut self) -> Self {
        if let Some(max_queue_size) = env::var(OTEL_BLRP_MAX_QUEUE_SIZE)
            .ok()
            .and_then(|queue_size| usize::from_str(&queue_size).ok())
        {
            self.max_queue_size = max_queue_size;
        }

        if let Some(max_export_batch_size) = env::var(OTEL_BLRP_MAX_EXPORT_BATCH_SIZE)
            .ok()
            .and_then(|batch_size| usize::from_str(&batch_size).ok())
        {
            self.max_export_batch_size = max_export_batch_size;
        }

        if let Some(scheduled_delay) = env::var(OTEL_BLRP_SCHEDULE_DELAY)
            .ok()
            .and_then(|delay| u64::from_str(&delay).ok())
        {
            self.scheduled_delay = Duration::from_millis(scheduled_delay);
        }

        if let Some(max_export_timeout) = env::var(OTEL_BLRP_EXPORT_TIMEOUT)
            .ok()
            .and_then(|s| u64::from_str(&s).ok())
        {
            self.max_export_timeout = Duration::from_millis(max_export_timeout);
        }

        self
    }
}

/// A builder for creating [`BatchLogProcessor`] instances.
///
#[derive(Debug)]
pub struct BatchLogProcessorBuilder<E, R> {
    exporter: E,
    config: BatchConfig,
    runtime: R,
}

impl<E, R> BatchLogProcessorBuilder<E, R>
where
    E: LogExporter + 'static,
    R: RuntimeChannel,
{
    /// Set the BatchConfig for [`BatchLogProcessorBuilder`]
    pub fn with_batch_config(self, config: BatchConfig) -> Self {
        BatchLogProcessorBuilder { config, ..self }
    }

    /// Build a batch processor
    pub fn build(self) -> BatchLogProcessor<R> {
        BatchLogProcessor::new(Box::new(self.exporter), self.config, self.runtime)
    }
}

/// Messages sent between application thread and batch log processor's work thread.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum BatchMessage {
    /// Export logs, usually called when the log is emitted.
    ExportLog((LogRecord, InstrumentationScope)),
    /// Flush the current buffer to the backend, it can be triggered by
    /// pre configured interval or a call to `force_push` function.
    Flush(Option<oneshot::Sender<ExportResult>>),
    /// Shut down the worker thread, push all logs in buffer to the backend.
    Shutdown(oneshot::Sender<ExportResult>),
    /// Set the resource for the exporter.
    SetResource(Arc<Resource>),
}

#[cfg(all(test, feature = "testing", feature = "logs"))]
mod tests {
    use super::{
        BatchLogProcessor, OTEL_BLRP_EXPORT_TIMEOUT, OTEL_BLRP_MAX_EXPORT_BATCH_SIZE,
        OTEL_BLRP_MAX_QUEUE_SIZE, OTEL_BLRP_SCHEDULE_DELAY,
    };
    use crate::export::logs::{LogBatch, LogExporter};
    use crate::logs::LogRecord;
    use crate::logs::LogResult;
    use crate::testing::logs::InMemoryLogExporterBuilder;
    use crate::{
        logs::{
            log_processor::{
                OTEL_BLRP_EXPORT_TIMEOUT_DEFAULT, OTEL_BLRP_MAX_EXPORT_BATCH_SIZE_DEFAULT,
                OTEL_BLRP_MAX_QUEUE_SIZE_DEFAULT, OTEL_BLRP_SCHEDULE_DELAY_DEFAULT,
            },
            BatchConfig, BatchConfigBuilder, LogProcessor, LoggerProvider, SimpleLogProcessor,
        },
        runtime,
        testing::logs::InMemoryLogExporter,
        Resource,
    };
    use async_trait::async_trait;
    use opentelemetry::logs::AnyValue;
    use opentelemetry::logs::LogRecord as _;
    use opentelemetry::logs::{Logger, LoggerProvider as _};
    use opentelemetry::KeyValue;
    use opentelemetry::{InstrumentationScope, Key};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Debug, Clone)]
    struct MockLogExporter {
        resource: Arc<Mutex<Option<Resource>>>,
    }

    #[async_trait]
    impl LogExporter for MockLogExporter {
        async fn export(&mut self, _batch: LogBatch<'_>) -> LogResult<()> {
            Ok(())
        }

        fn shutdown(&mut self) {}

        fn set_resource(&mut self, resource: &Resource) {
            self.resource
                .lock()
                .map(|mut res_opt| {
                    res_opt.replace(resource.clone());
                })
                .expect("mock log exporter shouldn't error when setting resource");
        }
    }

    // Implementation specific to the MockLogExporter, not part of the LogExporter trait
    impl MockLogExporter {
        fn get_resource(&self) -> Option<Resource> {
            (*self.resource).lock().unwrap().clone()
        }
    }

    #[test]
    fn test_default_const_values() {
        assert_eq!(OTEL_BLRP_SCHEDULE_DELAY, "OTEL_BLRP_SCHEDULE_DELAY");
        assert_eq!(OTEL_BLRP_SCHEDULE_DELAY_DEFAULT, 1_000);
        assert_eq!(OTEL_BLRP_EXPORT_TIMEOUT, "OTEL_BLRP_EXPORT_TIMEOUT");
        assert_eq!(OTEL_BLRP_EXPORT_TIMEOUT_DEFAULT, 30_000);
        assert_eq!(OTEL_BLRP_MAX_QUEUE_SIZE, "OTEL_BLRP_MAX_QUEUE_SIZE");
        assert_eq!(OTEL_BLRP_MAX_QUEUE_SIZE_DEFAULT, 2_048);
        assert_eq!(
            OTEL_BLRP_MAX_EXPORT_BATCH_SIZE,
            "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE"
        );
        assert_eq!(OTEL_BLRP_MAX_EXPORT_BATCH_SIZE_DEFAULT, 512);
    }

    #[test]
    fn test_default_batch_config_adheres_to_specification() {
        // The following environment variables are expected to be unset so that their default values are used.
        let env_vars = vec![
            OTEL_BLRP_SCHEDULE_DELAY,
            OTEL_BLRP_EXPORT_TIMEOUT,
            OTEL_BLRP_MAX_QUEUE_SIZE,
            OTEL_BLRP_MAX_EXPORT_BATCH_SIZE,
        ];

        let config = temp_env::with_vars_unset(env_vars, BatchConfig::default);

        assert_eq!(
            config.scheduled_delay,
            Duration::from_millis(OTEL_BLRP_SCHEDULE_DELAY_DEFAULT)
        );
        assert_eq!(
            config.max_export_timeout,
            Duration::from_millis(OTEL_BLRP_EXPORT_TIMEOUT_DEFAULT)
        );
        assert_eq!(config.max_queue_size, OTEL_BLRP_MAX_QUEUE_SIZE_DEFAULT);
        assert_eq!(
            config.max_export_batch_size,
            OTEL_BLRP_MAX_EXPORT_BATCH_SIZE_DEFAULT
        );
    }

    #[test]
    fn test_batch_config_configurable_by_env_vars() {
        let env_vars = vec![
            (OTEL_BLRP_SCHEDULE_DELAY, Some("2000")),
            (OTEL_BLRP_EXPORT_TIMEOUT, Some("60000")),
            (OTEL_BLRP_MAX_QUEUE_SIZE, Some("4096")),
            (OTEL_BLRP_MAX_EXPORT_BATCH_SIZE, Some("1024")),
        ];

        let config = temp_env::with_vars(env_vars, BatchConfig::default);

        assert_eq!(config.scheduled_delay, Duration::from_millis(2000));
        assert_eq!(config.max_export_timeout, Duration::from_millis(60000));
        assert_eq!(config.max_queue_size, 4096);
        assert_eq!(config.max_export_batch_size, 1024);
    }

    #[test]
    fn test_batch_config_max_export_batch_size_validation() {
        let env_vars = vec![
            (OTEL_BLRP_MAX_QUEUE_SIZE, Some("256")),
            (OTEL_BLRP_MAX_EXPORT_BATCH_SIZE, Some("1024")),
        ];

        let config = temp_env::with_vars(env_vars, BatchConfig::default);

        assert_eq!(config.max_queue_size, 256);
        assert_eq!(config.max_export_batch_size, 256);
        assert_eq!(
            config.scheduled_delay,
            Duration::from_millis(OTEL_BLRP_SCHEDULE_DELAY_DEFAULT)
        );
        assert_eq!(
            config.max_export_timeout,
            Duration::from_millis(OTEL_BLRP_EXPORT_TIMEOUT_DEFAULT)
        );
    }

    #[test]
    fn test_batch_config_with_fields() {
        let batch = BatchConfigBuilder::default()
            .with_max_export_batch_size(1)
            .with_scheduled_delay(Duration::from_millis(2))
            .with_max_export_timeout(Duration::from_millis(3))
            .with_max_queue_size(4)
            .build();

        assert_eq!(batch.max_export_batch_size, 1);
        assert_eq!(batch.scheduled_delay, Duration::from_millis(2));
        assert_eq!(batch.max_export_timeout, Duration::from_millis(3));
        assert_eq!(batch.max_queue_size, 4);
    }

    #[test]
    fn test_build_batch_log_processor_builder() {
        let mut env_vars = vec![
            (OTEL_BLRP_MAX_EXPORT_BATCH_SIZE, Some("500")),
            (OTEL_BLRP_SCHEDULE_DELAY, Some("I am not number")),
            (OTEL_BLRP_EXPORT_TIMEOUT, Some("2046")),
        ];
        temp_env::with_vars(env_vars.clone(), || {
            let builder =
                BatchLogProcessor::builder(InMemoryLogExporter::default(), runtime::Tokio);

            assert_eq!(builder.config.max_export_batch_size, 500);
            assert_eq!(
                builder.config.scheduled_delay,
                Duration::from_millis(OTEL_BLRP_SCHEDULE_DELAY_DEFAULT)
            );
            assert_eq!(
                builder.config.max_queue_size,
                OTEL_BLRP_MAX_QUEUE_SIZE_DEFAULT
            );
            assert_eq!(
                builder.config.max_export_timeout,
                Duration::from_millis(2046)
            );
        });

        env_vars.push((OTEL_BLRP_MAX_QUEUE_SIZE, Some("120")));

        temp_env::with_vars(env_vars, || {
            let builder =
                BatchLogProcessor::builder(InMemoryLogExporter::default(), runtime::Tokio);
            assert_eq!(builder.config.max_export_batch_size, 120);
            assert_eq!(builder.config.max_queue_size, 120);
        });
    }

    #[test]
    fn test_build_batch_log_processor_builder_with_custom_config() {
        let expected = BatchConfigBuilder::default()
            .with_max_export_batch_size(1)
            .with_scheduled_delay(Duration::from_millis(2))
            .with_max_export_timeout(Duration::from_millis(3))
            .with_max_queue_size(4)
            .build();

        let builder = BatchLogProcessor::builder(InMemoryLogExporter::default(), runtime::Tokio)
            .with_batch_config(expected);

        let actual = &builder.config;
        assert_eq!(actual.max_export_batch_size, 1);
        assert_eq!(actual.scheduled_delay, Duration::from_millis(2));
        assert_eq!(actual.max_export_timeout, Duration::from_millis(3));
        assert_eq!(actual.max_queue_size, 4);
    }

    #[test]
    fn test_set_resource_simple_processor() {
        let exporter = MockLogExporter {
            resource: Arc::new(Mutex::new(None)),
        };
        let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));
        let _ = LoggerProvider::builder()
            .with_log_processor(processor)
            .with_resource(Resource::new(vec![
                KeyValue::new("k1", "v1"),
                KeyValue::new("k2", "v3"),
                KeyValue::new("k3", "v3"),
                KeyValue::new("k4", "v4"),
                KeyValue::new("k5", "v5"),
            ]))
            .build();
        assert_eq!(exporter.get_resource().unwrap().into_iter().count(), 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_set_resource_batch_processor() {
        let exporter = MockLogExporter {
            resource: Arc::new(Mutex::new(None)),
        };
        let processor = BatchLogProcessor::new(
            Box::new(exporter.clone()),
            BatchConfig::default(),
            runtime::Tokio,
        );
        let provider = LoggerProvider::builder()
            .with_log_processor(processor)
            .with_resource(Resource::new(vec![
                KeyValue::new("k1", "v1"),
                KeyValue::new("k2", "v3"),
                KeyValue::new("k3", "v3"),
                KeyValue::new("k4", "v4"),
                KeyValue::new("k5", "v5"),
            ]))
            .build();
        tokio::time::sleep(Duration::from_secs(2)).await; // set resource in batch span processor is not blocking. Should we make it blocking?
        assert_eq!(exporter.get_resource().unwrap().into_iter().count(), 5);
        let _ = provider.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_batch_shutdown() {
        // assert we will receive an error
        // setup
        let exporter = InMemoryLogExporterBuilder::default()
            .keep_records_on_shutdown()
            .build();
        let processor = BatchLogProcessor::new(
            Box::new(exporter.clone()),
            BatchConfig::default(),
            runtime::Tokio,
        );

        let mut record = LogRecord::default();
        let instrumentation = InstrumentationScope::default();

        processor.emit(&mut record, &instrumentation);
        processor.force_flush().unwrap();
        processor.shutdown().unwrap();
        // todo: expect to see errors here. How should we assert this?
        processor.emit(&mut record, &instrumentation);
        assert_eq!(1, exporter.get_emitted_logs().unwrap().len())
    }

    #[test]
    fn test_simple_shutdown() {
        let exporter = InMemoryLogExporterBuilder::default()
            .keep_records_on_shutdown()
            .build();
        let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));

        let mut record: LogRecord = Default::default();
        let instrumentation: InstrumentationScope = Default::default();

        processor.emit(&mut record, &instrumentation);

        processor.shutdown().unwrap();

        let is_shutdown = processor
            .is_shutdown
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(is_shutdown);

        processor.emit(&mut record, &instrumentation);

        assert_eq!(1, exporter.get_emitted_logs().unwrap().len())
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "See issue https://github.com/open-telemetry/opentelemetry-rust/issues/1968"]
    async fn test_batch_log_processor_shutdown_with_async_runtime_current_flavor_multi_thread() {
        let exporter = InMemoryLogExporterBuilder::default().build();
        let processor = BatchLogProcessor::new(
            Box::new(exporter.clone()),
            BatchConfig::default(),
            runtime::Tokio,
        );

        //
        // deadloack happens in shutdown with tokio current_thread runtime
        //
        processor.shutdown().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_batch_log_processor_shutdown_with_async_runtime_current_flavor_current_thread() {
        let exporter = InMemoryLogExporterBuilder::default().build();
        let processor = BatchLogProcessor::new(
            Box::new(exporter.clone()),
            BatchConfig::default(),
            runtime::TokioCurrentThread,
        );

        processor.shutdown().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_batch_log_processor_shutdown_with_async_runtime_multi_flavor_multi_thread() {
        let exporter = InMemoryLogExporterBuilder::default().build();
        let processor = BatchLogProcessor::new(
            Box::new(exporter.clone()),
            BatchConfig::default(),
            runtime::Tokio,
        );

        processor.shutdown().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_batch_log_processor_shutdown_with_async_runtime_multi_flavor_current_thread() {
        let exporter = InMemoryLogExporterBuilder::default().build();
        let processor = BatchLogProcessor::new(
            Box::new(exporter.clone()),
            BatchConfig::default(),
            runtime::TokioCurrentThread,
        );

        processor.shutdown().unwrap();
    }

    #[derive(Debug)]
    struct FirstProcessor {
        pub(crate) logs: Arc<Mutex<Vec<(LogRecord, InstrumentationScope)>>>,
    }

    impl LogProcessor for FirstProcessor {
        fn emit(&self, record: &mut LogRecord, instrumentation: &InstrumentationScope) {
            // add attribute
            record.add_attribute(
                Key::from_static_str("processed_by"),
                AnyValue::String("FirstProcessor".into()),
            );
            // update body
            record.body = Some("Updated by FirstProcessor".into());

            self.logs
                .lock()
                .unwrap()
                .push((record.clone(), instrumentation.clone())); //clone as the LogProcessor is storing the data.
        }

        fn force_flush(&self) -> LogResult<()> {
            Ok(())
        }

        fn shutdown(&self) -> LogResult<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct SecondProcessor {
        pub(crate) logs: Arc<Mutex<Vec<(LogRecord, InstrumentationScope)>>>,
    }

    impl LogProcessor for SecondProcessor {
        fn emit(&self, record: &mut LogRecord, instrumentation: &InstrumentationScope) {
            assert!(record.attributes_contains(
                &Key::from_static_str("processed_by"),
                &AnyValue::String("FirstProcessor".into())
            ));
            assert!(
                record.body.clone().unwrap()
                    == AnyValue::String("Updated by FirstProcessor".into())
            );
            self.logs
                .lock()
                .unwrap()
                .push((record.clone(), instrumentation.clone()));
        }

        fn force_flush(&self) -> LogResult<()> {
            Ok(())
        }

        fn shutdown(&self) -> LogResult<()> {
            Ok(())
        }
    }
    #[test]
    fn test_log_data_modification_by_multiple_processors() {
        let first_processor_logs = Arc::new(Mutex::new(Vec::new()));
        let second_processor_logs = Arc::new(Mutex::new(Vec::new()));

        let first_processor = FirstProcessor {
            logs: Arc::clone(&first_processor_logs),
        };
        let second_processor = SecondProcessor {
            logs: Arc::clone(&second_processor_logs),
        };

        let logger_provider = LoggerProvider::builder()
            .with_log_processor(first_processor)
            .with_log_processor(second_processor)
            .build();

        let logger = logger_provider.logger("test-logger");
        let mut log_record = logger.create_log_record();
        log_record.body = Some(AnyValue::String("Test log".into()));

        logger.emit(log_record);

        assert_eq!(first_processor_logs.lock().unwrap().len(), 1);
        assert_eq!(second_processor_logs.lock().unwrap().len(), 1);

        let first_log = &first_processor_logs.lock().unwrap()[0];
        let second_log = &second_processor_logs.lock().unwrap()[0];

        assert!(first_log.0.attributes_contains(
            &Key::from_static_str("processed_by"),
            &AnyValue::String("FirstProcessor".into())
        ));
        assert!(second_log.0.attributes_contains(
            &Key::from_static_str("processed_by"),
            &AnyValue::String("FirstProcessor".into())
        ));

        assert!(
            first_log.0.body.clone().unwrap()
                == AnyValue::String("Updated by FirstProcessor".into())
        );
        assert!(
            second_log.0.body.clone().unwrap()
                == AnyValue::String("Updated by FirstProcessor".into())
        );
    }

    #[test]
    fn test_simple_processor_sync_exporter_without_runtime() {
        let exporter = InMemoryLogExporterBuilder::default().build();
        let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));

        let mut record: LogRecord = Default::default();
        let instrumentation: InstrumentationScope = Default::default();

        processor.emit(&mut record, &instrumentation);

        assert_eq!(exporter.get_emitted_logs().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_simple_processor_sync_exporter_with_runtime() {
        let exporter = InMemoryLogExporterBuilder::default().build();
        let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));

        let mut record: LogRecord = Default::default();
        let instrumentation: InstrumentationScope = Default::default();

        processor.emit(&mut record, &instrumentation);

        assert_eq!(exporter.get_emitted_logs().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_simple_processor_sync_exporter_with_multi_thread_runtime() {
        let exporter = InMemoryLogExporterBuilder::default().build();
        let processor = Arc::new(SimpleLogProcessor::new(Box::new(exporter.clone())));

        let mut handles = vec![];
        for _ in 0..10 {
            let processor_clone = Arc::clone(&processor);
            let handle = tokio::spawn(async move {
                let mut record: LogRecord = Default::default();
                let instrumentation: InstrumentationScope = Default::default();
                processor_clone.emit(&mut record, &instrumentation);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(exporter.get_emitted_logs().unwrap().len(), 10);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_simple_processor_sync_exporter_with_current_thread_runtime() {
        let exporter = InMemoryLogExporterBuilder::default().build();
        let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));

        let mut record: LogRecord = Default::default();
        let instrumentation: InstrumentationScope = Default::default();

        processor.emit(&mut record, &instrumentation);

        assert_eq!(exporter.get_emitted_logs().unwrap().len(), 1);
    }

    #[derive(Debug, Clone)]
    struct LogExporterThatRequiresTokio {
        export_count: Arc<AtomicUsize>,
    }

    impl LogExporterThatRequiresTokio {
        /// Creates a new instance of `LogExporterThatRequiresTokio`.
        fn new() -> Self {
            LogExporterThatRequiresTokio {
                export_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Returns the number of logs stored in the exporter.
        fn len(&self) -> usize {
            self.export_count.load(Ordering::Acquire)
        }
    }

    #[async_trait::async_trait]
    impl LogExporter for LogExporterThatRequiresTokio {
        async fn export(&mut self, batch: LogBatch<'_>) -> LogResult<()> {
            // Simulate minimal dependency on tokio by sleeping asynchronously for a short duration
            tokio::time::sleep(Duration::from_millis(50)).await;

            for _ in batch.iter() {
                self.export_count.fetch_add(1, Ordering::Acquire);
            }
            Ok(())
        }
    }

    #[test]
    fn test_simple_processor_async_exporter_without_runtime() {
        // Use `catch_unwind` to catch the panic caused by missing Tokio runtime
        let result = std::panic::catch_unwind(|| {
            let exporter = LogExporterThatRequiresTokio::new();
            let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));

            let mut record: LogRecord = Default::default();
            let instrumentation: InstrumentationScope = Default::default();

            // This will panic because an tokio async operation within exporter without a runtime.
            processor.emit(&mut record, &instrumentation);
        });

        // Verify that the panic occurred and check the panic message for the absence of a Tokio runtime
        assert!(
            result.is_err(),
            "The test should fail due to missing Tokio runtime, but it did not."
        );
        let panic_payload = result.unwrap_err();
        let panic_message = panic_payload
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| panic_payload.downcast_ref::<&str>().copied())
            .unwrap_or("No panic message");

        assert!(
            panic_message.contains("no reactor running")
                || panic_message.contains("must be called from the context of a Tokio 1.x runtime"),
            "Expected panic message about missing Tokio runtime, but got: {}",
            panic_message
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    // This test demonstrates a potential deadlock scenario in a multi-threaded Tokio runtime.
    // It spawns Tokio tasks equal to the number of runtime worker threads (4) to emit log events.
    // Each task attempts to acquire a mutex on the exporter in `SimpleLogProcessor::emit`.
    // Only one task obtains the lock, while the others are blocked, waiting for its release.
    //
    // The task holding the lock invokes the LogExporterThatRequiresTokio, which performs an
    // asynchronous operation (e.g., network I/O simulated by `tokio::sleep`). This operation
    // requires yielding control back to the Tokio runtime to make progress.
    //
    // However, all worker threads are occupied:
    // - One thread is executing the async exporter operation
    // - Three threads are blocked waiting for the mutex
    //
    // This leads to a deadlock as there are no available threads to drive the async operation
    // to completion, preventing the mutex from being released. Consequently, neither the blocked
    // tasks nor the exporter can proceed.
    async fn test_simple_processor_async_exporter_with_all_runtime_worker_threads_blocked() {
        let exporter = LogExporterThatRequiresTokio::new();
        let processor = Arc::new(SimpleLogProcessor::new(Box::new(exporter.clone())));

        let concurrent_emit = 4; // number of worker threads

        let mut handles = vec![];
        // try send `concurrent_emit` events concurrently
        for _ in 0..concurrent_emit {
            let processor_clone = Arc::clone(&processor);
            let handle = tokio::spawn(async move {
                let mut record: LogRecord = Default::default();
                let instrumentation: InstrumentationScope = Default::default();
                processor_clone.emit(&mut record, &instrumentation);
            });
            handles.push(handle);
        }

        // below code won't get executed
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(exporter.len(), concurrent_emit);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    // This test uses a multi-threaded runtime setup with a single worker thread. Note that even
    // though only one worker thread is created, it is distinct from the main thread. The processor
    // emits a log event, and the exporter performs an async operation that requires the runtime.
    // The single worker thread handles this operation without deadlocking, as long as no other
    // tasks occupy the runtime.
    async fn test_simple_processor_async_exporter_with_runtime() {
        let exporter = LogExporterThatRequiresTokio::new();
        let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));

        let mut record: LogRecord = Default::default();
        let instrumentation: InstrumentationScope = Default::default();

        processor.emit(&mut record, &instrumentation);

        assert_eq!(exporter.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    // This test uses a multi-threaded runtime setup with the default number of worker threads.
    // The processor emits a log event, and the exporter, which requires the runtime for its async
    // operations, can access one of the available worker threads to complete its task. As there
    // are multiple threads, the exporter can proceed without blocking other tasks, ensuring the
    // test completes successfully.
    async fn test_simple_processor_async_exporter_with_multi_thread_runtime() {
        let exporter = LogExporterThatRequiresTokio::new();

        let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));

        let mut record: LogRecord = Default::default();
        let instrumentation: InstrumentationScope = Default::default();

        processor.emit(&mut record, &instrumentation);

        assert_eq!(exporter.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    // This test uses a current-thread runtime, where all operations run on the main thread.
    // The processor emits a log event while the runtime is blocked using `futures::block_on`
    // to complete the export operation. The exporter, which performs an async operation and
    // requires the runtime, cannot progress because the main thread is already blocked.
    // This results in a deadlock, as the runtime cannot move forward.
    async fn test_simple_processor_async_exporter_with_current_thread_runtime() {
        let exporter = LogExporterThatRequiresTokio::new();

        let processor = SimpleLogProcessor::new(Box::new(exporter.clone()));

        let mut record: LogRecord = Default::default();
        let instrumentation: InstrumentationScope = Default::default();

        processor.emit(&mut record, &instrumentation);

        assert_eq!(exporter.len(), 1);
    }
}
