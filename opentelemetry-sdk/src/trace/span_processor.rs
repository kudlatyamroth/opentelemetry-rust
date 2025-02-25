//! # OpenTelemetry Span Processor Interface
//!
//! Span processor is an interface which allows hooks for span start and end method
//! invocations. The span processors are invoked only when
//! [`is_recording`] is true.
//!
//! Built-in span processors are responsible for batching and conversion of spans to
//! exportable representation and passing batches to exporters.
//!
//! Span processors can be registered directly on SDK [`TracerProvider`] and they are
//! invoked in the same order as they were registered.
//!
//! All `Tracer` instances created by a `TracerProvider` share the same span processors.
//! Changes to this collection reflect in all `Tracer` instances.
//!
//! The following diagram shows `SpanProcessor`'s relationship to other components
//! in the SDK:
//!
//! ```ascii
//!   +-----+--------------+   +-----------------------+   +-------------------+
//!   |     |              |   |                       |   |                   |
//!   |     |              |   | (Batch)SpanProcessor  |   |    SpanExporter   |
//!   |     |              +---> (Simple)SpanProcessor +--->  (OTLPExporter)   |
//!   |     |              |   |                       |   |                   |
//!   | SDK | Tracer.span()|   +-----------------------+   +-------------------+
//!   |     | Span.end()   |
//!   |     |              |
//!   |     |              |
//!   |     |              |
//!   |     |              |
//!   +-----+--------------+
//! ```
//!
//! [`is_recording`]: opentelemetry::trace::Span::is_recording()
//! [`TracerProvider`]: opentelemetry::trace::TracerProvider

use crate::export::trace::{ExportResult, SpanData, SpanExporter};
use crate::resource::Resource;
use crate::runtime::{RuntimeChannel, TrySend};
use crate::trace::Span;
use futures_channel::oneshot;
use futures_util::{
    future::{self, BoxFuture, Either},
    select,
    stream::{self, FusedStream, FuturesUnordered},
    StreamExt as _,
};
use opentelemetry::{otel_debug, otel_error};
use opentelemetry::{
    trace::{TraceError, TraceResult},
    Context,
};
use std::cmp::min;
use std::sync::{Arc, Mutex};
use std::{env, fmt, str::FromStr, time::Duration};

/// Delay interval between two consecutive exports.
const OTEL_BSP_SCHEDULE_DELAY: &str = "OTEL_BSP_SCHEDULE_DELAY";
/// Default delay interval between two consecutive exports.
const OTEL_BSP_SCHEDULE_DELAY_DEFAULT: u64 = 5_000;
/// Maximum queue size
const OTEL_BSP_MAX_QUEUE_SIZE: &str = "OTEL_BSP_MAX_QUEUE_SIZE";
/// Default maximum queue size
const OTEL_BSP_MAX_QUEUE_SIZE_DEFAULT: usize = 2_048;
/// Maximum batch size, must be less than or equal to OTEL_BSP_MAX_QUEUE_SIZE
const OTEL_BSP_MAX_EXPORT_BATCH_SIZE: &str = "OTEL_BSP_MAX_EXPORT_BATCH_SIZE";
/// Default maximum batch size
const OTEL_BSP_MAX_EXPORT_BATCH_SIZE_DEFAULT: usize = 512;
/// Maximum allowed time to export data.
const OTEL_BSP_EXPORT_TIMEOUT: &str = "OTEL_BSP_EXPORT_TIMEOUT";
/// Default maximum allowed time to export data.
const OTEL_BSP_EXPORT_TIMEOUT_DEFAULT: u64 = 30_000;
/// Environment variable to configure max concurrent exports for batch span
/// processor.
const OTEL_BSP_MAX_CONCURRENT_EXPORTS: &str = "OTEL_BSP_MAX_CONCURRENT_EXPORTS";
/// Default max concurrent exports for BSP
const OTEL_BSP_MAX_CONCURRENT_EXPORTS_DEFAULT: usize = 1;

/// `SpanProcessor` is an interface which allows hooks for span start and end
/// method invocations. The span processors are invoked only when is_recording
/// is true.
pub trait SpanProcessor: Send + Sync + std::fmt::Debug {
    /// `on_start` is called when a `Span` is started.  This method is called
    /// synchronously on the thread that started the span, therefore it should
    /// not block or throw exceptions.
    fn on_start(&self, span: &mut Span, cx: &Context);
    /// `on_end` is called after a `Span` is ended (i.e., the end timestamp is
    /// already set). This method is called synchronously within the `Span::end`
    /// API, therefore it should not block or throw an exception.
    fn on_end(&self, span: SpanData);
    /// Force the spans lying in the cache to be exported.
    fn force_flush(&self) -> TraceResult<()>;
    /// Shuts down the processor. Called when SDK is shut down. This is an
    /// opportunity for processors to do any cleanup required.
    ///
    /// Implementation should make sure shutdown can be called multiple times.
    fn shutdown(&self) -> TraceResult<()>;
    /// Set the resource for the log processor.
    fn set_resource(&mut self, _resource: &Resource) {}
}

/// A [SpanProcessor] that passes finished spans to the configured
/// `SpanExporter`, as soon as they are finished, without any batching. This is
/// typically useful for debugging and testing. For scenarios requiring higher
/// performance/throughput, consider using [BatchSpanProcessor].
#[derive(Debug)]
pub struct SimpleSpanProcessor {
    exporter: Mutex<Box<dyn SpanExporter>>,
}

impl SimpleSpanProcessor {
    /// Create a new [SimpleSpanProcessor] using the provided exporter.
    pub fn new(exporter: Box<dyn SpanExporter>) -> Self {
        Self {
            exporter: Mutex::new(exporter),
        }
    }
}

impl SpanProcessor for SimpleSpanProcessor {
    fn on_start(&self, _span: &mut Span, _cx: &Context) {
        // Ignored
    }

    fn on_end(&self, span: SpanData) {
        if !span.span_context.is_sampled() {
            return;
        }

        let result = self
            .exporter
            .lock()
            .map_err(|_| TraceError::Other("SimpleSpanProcessor mutex poison".into()))
            .and_then(|mut exporter| futures_executor::block_on(exporter.export(vec![span])));

        if let Err(err) = result {
            // TODO: check error type, and log `error` only if the error is user-actiobable, else log `debug`
            otel_debug!(
                name: "SimpleProcessor.OnEnd.Error",
                reason = format!("{:?}", err)
            );
        }
    }

    fn force_flush(&self) -> TraceResult<()> {
        // Nothing to flush for simple span processor.
        Ok(())
    }

    fn shutdown(&self) -> TraceResult<()> {
        if let Ok(mut exporter) = self.exporter.lock() {
            exporter.shutdown();
            Ok(())
        } else {
            Err(TraceError::Other(
                "SimpleSpanProcessor mutex poison at shutdown".into(),
            ))
        }
    }

    fn set_resource(&mut self, resource: &Resource) {
        if let Ok(mut exporter) = self.exporter.lock() {
            exporter.set_resource(resource);
        }
    }
}

/// A [`SpanProcessor`] that asynchronously buffers finished spans and reports
/// them at a preconfigured interval.
///
/// Batch span processors need to run a background task to collect and send
/// spans. Different runtimes need different ways to handle the background task.
///
/// Note: Configuring an opentelemetry `Runtime` that's not compatible with the
/// underlying runtime can cause deadlocks (see tokio section).
///
/// ### Use with Tokio
///
/// Tokio currently offers two different schedulers. One is
/// `current_thread_scheduler`, the other is `multiple_thread_scheduler`. Both
/// of them default to use batch span processors to install span exporters.
///
/// Tokio's `current_thread_scheduler` can cause the program to hang forever if
/// blocking work is scheduled with other tasks in the same runtime. To avoid
/// this, be sure to enable the `rt-tokio-current-thread` feature in this crate
/// if you are using that runtime (e.g. users of actix-web), and blocking tasks
/// will then be scheduled on a different thread.
///
/// # Examples
///
/// This processor can be configured with an [`executor`] of your choice to
/// batch and upload spans asynchronously when they end. If you have added a
/// library like [`tokio`] or [`async-std`], you can pass in their respective
/// `spawn` and `interval` functions to have batching performed in those
/// contexts.
///
/// ```
/// # #[cfg(feature="tokio")]
/// # {
/// use opentelemetry::global;
/// use opentelemetry_sdk::{runtime, testing::trace::NoopSpanExporter, trace};
/// use opentelemetry_sdk::trace::BatchConfigBuilder;
/// use std::time::Duration;
///
/// #[tokio::main]
/// async fn main() {
///     // Configure your preferred exporter
///     let exporter = NoopSpanExporter::new();
///
///     // Create a batch span processor using an exporter and a runtime
///     let batch = trace::BatchSpanProcessor::builder(exporter, runtime::Tokio)
///         .with_batch_config(BatchConfigBuilder::default().with_max_queue_size(4096).build())
///         .build();
///
///     // Then use the `with_batch_exporter` method to have the provider export spans in batches.
///     let provider = trace::TracerProvider::builder()
///         .with_span_processor(batch)
///         .build();
///
///     let _ = global::set_tracer_provider(provider);
/// }
/// # }
/// ```
///
/// [`executor`]: https://docs.rs/futures/0.3/futures/executor/index.html
/// [`tokio`]: https://tokio.rs
/// [`async-std`]: https://async.rs
pub struct BatchSpanProcessor<R: RuntimeChannel> {
    message_sender: R::Sender<BatchMessage>,
}

impl<R: RuntimeChannel> fmt::Debug for BatchSpanProcessor<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchSpanProcessor")
            .field("message_sender", &self.message_sender)
            .finish()
    }
}

impl<R: RuntimeChannel> SpanProcessor for BatchSpanProcessor<R> {
    fn on_start(&self, _span: &mut Span, _cx: &Context) {
        // Ignored
    }

    fn on_end(&self, span: SpanData) {
        if !span.span_context.is_sampled() {
            return;
        }

        let result = self.message_sender.try_send(BatchMessage::ExportSpan(span));

        if let Err(err) = result {
            otel_debug!(
                name: "BatchSpanProcessor.OnEnd.ExportQueueingFailed",
                reason = format!("{:?}", TraceError::Other(err.into()))
            );
        }
    }

    fn force_flush(&self) -> TraceResult<()> {
        let (res_sender, res_receiver) = oneshot::channel();
        self.message_sender
            .try_send(BatchMessage::Flush(Some(res_sender)))
            .map_err(|err| TraceError::Other(err.into()))?;

        futures_executor::block_on(res_receiver)
            .map_err(|err| TraceError::Other(err.into()))
            .and_then(|identity| identity)
    }

    fn shutdown(&self) -> TraceResult<()> {
        let (res_sender, res_receiver) = oneshot::channel();
        self.message_sender
            .try_send(BatchMessage::Shutdown(res_sender))
            .map_err(|err| TraceError::Other(err.into()))?;

        futures_executor::block_on(res_receiver)
            .map_err(|err| TraceError::Other(err.into()))
            .and_then(|identity| identity)
    }

    fn set_resource(&mut self, resource: &Resource) {
        let resource = Arc::new(resource.clone());
        let _ = self
            .message_sender
            .try_send(BatchMessage::SetResource(resource));
    }
}

/// Messages sent between application thread and batch span processor's work thread.
// In this enum the size difference is not a concern because:
// 1. If we wrap SpanData into a pointer, it will add overhead when processing.
// 2. Most of the messages will be ExportSpan.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum BatchMessage {
    /// Export spans, usually called when span ends
    ExportSpan(SpanData),
    /// Flush the current buffer to the backend, it can be triggered by
    /// pre configured interval or a call to `force_push` function.
    Flush(Option<oneshot::Sender<ExportResult>>),
    /// Shut down the worker thread, push all spans in buffer to the backend.
    Shutdown(oneshot::Sender<ExportResult>),
    /// Set the resource for the exporter.
    SetResource(Arc<Resource>),
}

struct BatchSpanProcessorInternal<R> {
    spans: Vec<SpanData>,
    export_tasks: FuturesUnordered<BoxFuture<'static, ExportResult>>,
    runtime: R,
    exporter: Box<dyn SpanExporter>,
    config: BatchConfig,
}

impl<R: RuntimeChannel> BatchSpanProcessorInternal<R> {
    async fn flush(&mut self, res_channel: Option<oneshot::Sender<ExportResult>>) {
        let export_task = self.export();
        let task = Box::pin(async move {
            let result = export_task.await;

            if let Some(channel) = res_channel {
                // If a response channel is provided, attempt to send the export result through it.
                if let Err(result) = channel.send(result) {
                    otel_debug!(
                        name: "BatchSpanProcessor.Flush.SendResultError",
                        reason = format!("{:?}", result)
                    );
                }
            } else if let Err(err) = result {
                // If no channel is provided and the export operation encountered an error,
                // log the error directly here.
                // TODO: Consider returning the status instead of logging it.
                otel_error!(
                    name: "BatchSpanProcessor.Flush.ExportError",
                    reason = format!("{:?}", err),
                    message = "Failed during the export process"
                );
            }

            Ok(())
        });

        if self.config.max_concurrent_exports == 1 {
            let _ = task.await;
        } else {
            self.export_tasks.push(task);
            while self.export_tasks.next().await.is_some() {}
        }
    }

    /// Process a single message
    ///
    /// A return value of false indicates shutdown
    async fn process_message(&mut self, message: BatchMessage) -> bool {
        match message {
            // Span has finished, add to buffer of pending spans.
            BatchMessage::ExportSpan(span) => {
                self.spans.push(span);

                if self.spans.len() == self.config.max_export_batch_size {
                    // If concurrent exports are saturated, wait for one to complete.
                    if !self.export_tasks.is_empty()
                        && self.export_tasks.len() == self.config.max_concurrent_exports
                    {
                        self.export_tasks.next().await;
                    }

                    let export_task = self.export();
                    let task = async move {
                        if let Err(err) = export_task.await {
                            otel_error!(
                                name: "BatchSpanProcessor.Export.Error",
                                reason = format!("{}", err)
                            );
                        }

                        Ok(())
                    };
                    // Special case when not using concurrent exports
                    if self.config.max_concurrent_exports == 1 {
                        let _ = task.await;
                    } else {
                        self.export_tasks.push(Box::pin(task));
                    }
                }
            }
            // Span batch interval time reached or a force flush has been invoked, export
            // current spans.
            //
            // This is a hint to ensure that any tasks associated with Spans for which the
            // SpanProcessor had already received events prior to the call to ForceFlush
            // SHOULD be completed as soon as possible, preferably before returning from
            // this method.
            //
            // In particular, if any SpanProcessor has any associated exporter, it SHOULD
            // try to call the exporter's Export with all spans for which this was not
            // already done and then invoke ForceFlush on it. The built-in SpanProcessors
            // MUST do so. If a timeout is specified (see below), the SpanProcessor MUST
            // prioritize honoring the timeout over finishing all calls. It MAY skip or
            // abort some or all Export or ForceFlush calls it has made to achieve this
            // goal.
            //
            // NB: `force_flush` is not currently implemented on exporters; the equivalent
            // would be waiting for exporter tasks to complete. In the case of
            // channel-coupled exporters, they will need a `force_flush` implementation to
            // properly block.
            BatchMessage::Flush(res_channel) => {
                self.flush(res_channel).await;
            }
            // Stream has terminated or processor is shutdown, return to finish execution.
            BatchMessage::Shutdown(ch) => {
                self.flush(Some(ch)).await;
                self.exporter.shutdown();
                return false;
            }
            // propagate the resource
            BatchMessage::SetResource(resource) => {
                self.exporter.set_resource(&resource);
            }
        }
        true
    }

    fn export(&mut self) -> BoxFuture<'static, ExportResult> {
        // Batch size check for flush / shutdown. Those methods may be called
        // when there's no work to do.
        if self.spans.is_empty() {
            return Box::pin(future::ready(Ok(())));
        }

        let export = self.exporter.export(self.spans.split_off(0));
        let timeout = self.runtime.delay(self.config.max_export_timeout);
        let time_out = self.config.max_export_timeout;

        Box::pin(async move {
            match future::select(export, timeout).await {
                Either::Left((export_res, _)) => export_res,
                Either::Right((_, _)) => ExportResult::Err(TraceError::ExportTimedOut(time_out)),
            }
        })
    }

    async fn run(mut self, mut messages: impl FusedStream<Item = BatchMessage> + Unpin) {
        loop {
            select! {
                // FuturesUnordered implements Fuse intelligently such that it
                // will become eligible again once new tasks are added to it.
                _ = self.export_tasks.next() => {
                    // An export task completed; do we need to do anything with it?
                },
                message = messages.next() => {
                    match message {
                        Some(message) => {
                            if !self.process_message(message).await {
                                break;
                            }
                        },
                        None => break,
                    }
                },
            }
        }
    }
}

impl<R: RuntimeChannel> BatchSpanProcessor<R> {
    pub(crate) fn new(exporter: Box<dyn SpanExporter>, config: BatchConfig, runtime: R) -> Self {
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

            let messages = Box::pin(stream::select(message_receiver, ticker));
            let processor = BatchSpanProcessorInternal {
                spans: Vec::new(),
                export_tasks: FuturesUnordered::new(),
                runtime: timeout_runtime,
                config,
                exporter,
            };

            processor.run(messages).await
        }));

        // Return batch processor with link to worker
        BatchSpanProcessor { message_sender }
    }

    /// Create a new batch processor builder
    pub fn builder<E>(exporter: E, runtime: R) -> BatchSpanProcessorBuilder<E, R>
    where
        E: SpanExporter,
    {
        BatchSpanProcessorBuilder {
            exporter,
            config: Default::default(),
            runtime,
        }
    }
}

/// Batch span processor configuration.
/// Use [`BatchConfigBuilder`] to configure your own instance of [`BatchConfig`].
#[derive(Debug)]
pub struct BatchConfig {
    /// The maximum queue size to buffer spans for delayed processing. If the
    /// queue gets full it drops the spans. The default value of is 2048.
    max_queue_size: usize,

    /// The delay interval in milliseconds between two consecutive processing
    /// of batches. The default value is 5 seconds.
    scheduled_delay: Duration,

    /// The maximum number of spans to process in a single batch. If there are
    /// more than one batch worth of spans then it processes multiple batches
    /// of spans one batch after the other without any delay. The default value
    /// is 512.
    max_export_batch_size: usize,

    /// The maximum duration to export a batch of data.
    max_export_timeout: Duration,

    /// Maximum number of concurrent exports
    ///
    /// Limits the number of spawned tasks for exports and thus memory consumed
    /// by an exporter. A value of 1 will cause exports to be performed
    /// synchronously on the BatchSpanProcessor task.
    max_concurrent_exports: usize,
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
    max_concurrent_exports: usize,
}

impl Default for BatchConfigBuilder {
    /// Create a new [`BatchConfigBuilder`] initialized with default batch config values as per the specs.
    /// The values are overriden by environment variables if set.
    /// The supported environment variables are:
    /// * `OTEL_BSP_MAX_QUEUE_SIZE`
    /// * `OTEL_BSP_SCHEDULE_DELAY`
    /// * `OTEL_BSP_MAX_EXPORT_BATCH_SIZE`
    /// * `OTEL_BSP_EXPORT_TIMEOUT`
    /// * `OTEL_BSP_MAX_CONCURRENT_EXPORTS`
    fn default() -> Self {
        BatchConfigBuilder {
            max_queue_size: OTEL_BSP_MAX_QUEUE_SIZE_DEFAULT,
            scheduled_delay: Duration::from_millis(OTEL_BSP_SCHEDULE_DELAY_DEFAULT),
            max_export_batch_size: OTEL_BSP_MAX_EXPORT_BATCH_SIZE_DEFAULT,
            max_export_timeout: Duration::from_millis(OTEL_BSP_EXPORT_TIMEOUT_DEFAULT),
            max_concurrent_exports: OTEL_BSP_MAX_CONCURRENT_EXPORTS_DEFAULT,
        }
        .init_from_env_vars()
    }
}

impl BatchConfigBuilder {
    /// Set max_queue_size for [`BatchConfigBuilder`].
    /// It's the maximum queue size to buffer spans for delayed processing.
    /// If the queue gets full it will drops the spans.
    /// The default value of is 2048.
    pub fn with_max_queue_size(mut self, max_queue_size: usize) -> Self {
        self.max_queue_size = max_queue_size;
        self
    }

    /// Set max_export_batch_size for [`BatchConfigBuilder`].
    /// It's the maximum number of spans to process in a single batch. If there are
    /// more than one batch worth of spans then it processes multiple batches
    /// of spans one batch after the other without any delay. The default value
    /// is 512.
    pub fn with_max_export_batch_size(mut self, max_export_batch_size: usize) -> Self {
        self.max_export_batch_size = max_export_batch_size;
        self
    }

    /// Set max_concurrent_exports for [`BatchConfigBuilder`].
    /// It's the maximum number of concurrent exports.
    /// Limits the number of spawned tasks for exports and thus memory consumed by an exporter.
    /// The default value is 1.
    /// IF the max_concurrent_exports value is default value, it will cause exports to be performed
    /// synchronously on the BatchSpanProcessor task.
    pub fn with_max_concurrent_exports(mut self, max_concurrent_exports: usize) -> Self {
        self.max_concurrent_exports = max_concurrent_exports;
        self
    }

    /// Set scheduled_delay_duration for [`BatchConfigBuilder`].
    /// It's the delay interval in milliseconds between two consecutive processing of batches.
    /// The default value is 5000 milliseconds.
    pub fn with_scheduled_delay(mut self, scheduled_delay: Duration) -> Self {
        self.scheduled_delay = scheduled_delay;
        self
    }

    /// Set max_export_timeout for [`BatchConfigBuilder`].
    /// It's the maximum duration to export a batch of data.
    /// The The default value is 30000 milliseconds.
    pub fn with_max_export_timeout(mut self, max_export_timeout: Duration) -> Self {
        self.max_export_timeout = max_export_timeout;
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
            max_concurrent_exports: self.max_concurrent_exports,
            max_export_batch_size,
        }
    }

    fn init_from_env_vars(mut self) -> Self {
        if let Some(max_concurrent_exports) = env::var(OTEL_BSP_MAX_CONCURRENT_EXPORTS)
            .ok()
            .and_then(|max_concurrent_exports| usize::from_str(&max_concurrent_exports).ok())
        {
            self.max_concurrent_exports = max_concurrent_exports;
        }

        if let Some(max_queue_size) = env::var(OTEL_BSP_MAX_QUEUE_SIZE)
            .ok()
            .and_then(|queue_size| usize::from_str(&queue_size).ok())
        {
            self.max_queue_size = max_queue_size;
        }

        if let Some(scheduled_delay) = env::var(OTEL_BSP_SCHEDULE_DELAY)
            .ok()
            .and_then(|delay| u64::from_str(&delay).ok())
        {
            self.scheduled_delay = Duration::from_millis(scheduled_delay);
        }

        if let Some(max_export_batch_size) = env::var(OTEL_BSP_MAX_EXPORT_BATCH_SIZE)
            .ok()
            .and_then(|batch_size| usize::from_str(&batch_size).ok())
        {
            self.max_export_batch_size = max_export_batch_size;
        }

        // max export batch size must be less or equal to max queue size.
        // we set max export batch size to max queue size if it's larger than max queue size.
        if self.max_export_batch_size > self.max_queue_size {
            self.max_export_batch_size = self.max_queue_size;
        }

        if let Some(max_export_timeout) = env::var(OTEL_BSP_EXPORT_TIMEOUT)
            .ok()
            .and_then(|timeout| u64::from_str(&timeout).ok())
        {
            self.max_export_timeout = Duration::from_millis(max_export_timeout);
        }

        self
    }
}

/// A builder for creating [`BatchSpanProcessor`] instances.
///
#[derive(Debug)]
pub struct BatchSpanProcessorBuilder<E, R> {
    exporter: E,
    config: BatchConfig,
    runtime: R,
}

impl<E, R> BatchSpanProcessorBuilder<E, R>
where
    E: SpanExporter + 'static,
    R: RuntimeChannel,
{
    /// Set the BatchConfig for [BatchSpanProcessorBuilder]
    pub fn with_batch_config(self, config: BatchConfig) -> Self {
        BatchSpanProcessorBuilder { config, ..self }
    }

    /// Build a batch processor
    pub fn build(self) -> BatchSpanProcessor<R> {
        BatchSpanProcessor::new(Box::new(self.exporter), self.config, self.runtime)
    }
}

#[cfg(all(test, feature = "testing", feature = "trace"))]
mod tests {
    // cargo test trace::span_processor::tests:: --features=testing
    use super::{
        BatchSpanProcessor, SimpleSpanProcessor, SpanProcessor, OTEL_BSP_EXPORT_TIMEOUT,
        OTEL_BSP_MAX_EXPORT_BATCH_SIZE, OTEL_BSP_MAX_QUEUE_SIZE, OTEL_BSP_MAX_QUEUE_SIZE_DEFAULT,
        OTEL_BSP_SCHEDULE_DELAY, OTEL_BSP_SCHEDULE_DELAY_DEFAULT,
    };
    use crate::export::trace::{ExportResult, SpanData, SpanExporter};
    use crate::runtime;
    use crate::testing::trace::{
        new_test_export_span_data, new_tokio_test_exporter, InMemorySpanExporterBuilder,
    };
    use crate::trace::span_processor::{
        OTEL_BSP_EXPORT_TIMEOUT_DEFAULT, OTEL_BSP_MAX_CONCURRENT_EXPORTS,
        OTEL_BSP_MAX_CONCURRENT_EXPORTS_DEFAULT, OTEL_BSP_MAX_EXPORT_BATCH_SIZE_DEFAULT,
    };
    use crate::trace::{BatchConfig, BatchConfigBuilder, SpanEvents, SpanLinks};
    use async_trait::async_trait;
    use opentelemetry::trace::{SpanContext, SpanId, SpanKind, Status};
    use std::fmt::Debug;
    use std::future::Future;
    use std::time::Duration;

    #[test]
    fn simple_span_processor_on_end_calls_export() {
        let exporter = InMemorySpanExporterBuilder::new().build();
        let processor = SimpleSpanProcessor::new(Box::new(exporter.clone()));
        let span_data = new_test_export_span_data();
        processor.on_end(span_data.clone());
        assert_eq!(exporter.get_finished_spans().unwrap()[0], span_data);
        let _result = processor.shutdown();
    }

    #[test]
    fn simple_span_processor_on_end_skips_export_if_not_sampled() {
        let exporter = InMemorySpanExporterBuilder::new().build();
        let processor = SimpleSpanProcessor::new(Box::new(exporter.clone()));
        let unsampled = SpanData {
            span_context: SpanContext::empty_context(),
            parent_span_id: SpanId::INVALID,
            span_kind: SpanKind::Internal,
            name: "opentelemetry".into(),
            start_time: opentelemetry::time::now(),
            end_time: opentelemetry::time::now(),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: Status::Unset,
            instrumentation_scope: Default::default(),
        };
        processor.on_end(unsampled);
        assert!(exporter.get_finished_spans().unwrap().is_empty());
    }

    #[test]
    fn simple_span_processor_shutdown_calls_shutdown() {
        let exporter = InMemorySpanExporterBuilder::new().build();
        let processor = SimpleSpanProcessor::new(Box::new(exporter.clone()));
        let span_data = new_test_export_span_data();
        processor.on_end(span_data.clone());
        assert!(!exporter.get_finished_spans().unwrap().is_empty());
        let _result = processor.shutdown();
        // Assume shutdown is called by ensuring spans are empty in the exporter
        assert!(exporter.get_finished_spans().unwrap().is_empty());
    }

    #[test]
    fn test_default_const_values() {
        assert_eq!(OTEL_BSP_MAX_QUEUE_SIZE, "OTEL_BSP_MAX_QUEUE_SIZE");
        assert_eq!(OTEL_BSP_MAX_QUEUE_SIZE_DEFAULT, 2048);
        assert_eq!(OTEL_BSP_SCHEDULE_DELAY, "OTEL_BSP_SCHEDULE_DELAY");
        assert_eq!(OTEL_BSP_SCHEDULE_DELAY_DEFAULT, 5000);
        assert_eq!(
            OTEL_BSP_MAX_EXPORT_BATCH_SIZE,
            "OTEL_BSP_MAX_EXPORT_BATCH_SIZE"
        );
        assert_eq!(OTEL_BSP_MAX_EXPORT_BATCH_SIZE_DEFAULT, 512);
        assert_eq!(OTEL_BSP_EXPORT_TIMEOUT, "OTEL_BSP_EXPORT_TIMEOUT");
        assert_eq!(OTEL_BSP_EXPORT_TIMEOUT_DEFAULT, 30000);
    }

    #[test]
    fn test_default_batch_config_adheres_to_specification() {
        let env_vars = vec![
            OTEL_BSP_SCHEDULE_DELAY,
            OTEL_BSP_EXPORT_TIMEOUT,
            OTEL_BSP_MAX_QUEUE_SIZE,
            OTEL_BSP_MAX_EXPORT_BATCH_SIZE,
            OTEL_BSP_MAX_CONCURRENT_EXPORTS,
        ];

        let config = temp_env::with_vars_unset(env_vars, BatchConfig::default);

        assert_eq!(
            config.max_concurrent_exports,
            OTEL_BSP_MAX_CONCURRENT_EXPORTS_DEFAULT
        );
        assert_eq!(
            config.scheduled_delay,
            Duration::from_millis(OTEL_BSP_SCHEDULE_DELAY_DEFAULT)
        );
        assert_eq!(
            config.max_export_timeout,
            Duration::from_millis(OTEL_BSP_EXPORT_TIMEOUT_DEFAULT)
        );
        assert_eq!(config.max_queue_size, OTEL_BSP_MAX_QUEUE_SIZE_DEFAULT);
        assert_eq!(
            config.max_export_batch_size,
            OTEL_BSP_MAX_EXPORT_BATCH_SIZE_DEFAULT
        );
    }

    #[test]
    fn test_batch_config_configurable_by_env_vars() {
        let env_vars = vec![
            (OTEL_BSP_SCHEDULE_DELAY, Some("2000")),
            (OTEL_BSP_EXPORT_TIMEOUT, Some("60000")),
            (OTEL_BSP_MAX_QUEUE_SIZE, Some("4096")),
            (OTEL_BSP_MAX_EXPORT_BATCH_SIZE, Some("1024")),
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
            (OTEL_BSP_MAX_QUEUE_SIZE, Some("256")),
            (OTEL_BSP_MAX_EXPORT_BATCH_SIZE, Some("1024")),
        ];

        let config = temp_env::with_vars(env_vars, BatchConfig::default);

        assert_eq!(config.max_queue_size, 256);
        assert_eq!(config.max_export_batch_size, 256);
        assert_eq!(
            config.scheduled_delay,
            Duration::from_millis(OTEL_BSP_SCHEDULE_DELAY_DEFAULT)
        );
        assert_eq!(
            config.max_export_timeout,
            Duration::from_millis(OTEL_BSP_EXPORT_TIMEOUT_DEFAULT)
        );
    }

    #[test]
    fn test_batch_config_with_fields() {
        let batch = BatchConfigBuilder::default()
            .with_max_export_batch_size(10)
            .with_scheduled_delay(Duration::from_millis(10))
            .with_max_export_timeout(Duration::from_millis(10))
            .with_max_concurrent_exports(10)
            .with_max_queue_size(10)
            .build();
        assert_eq!(batch.max_export_batch_size, 10);
        assert_eq!(batch.scheduled_delay, Duration::from_millis(10));
        assert_eq!(batch.max_export_timeout, Duration::from_millis(10));
        assert_eq!(batch.max_concurrent_exports, 10);
        assert_eq!(batch.max_queue_size, 10);
    }

    #[test]
    fn test_build_batch_span_processor_builder() {
        let mut env_vars = vec![
            (OTEL_BSP_MAX_EXPORT_BATCH_SIZE, Some("500")),
            (OTEL_BSP_SCHEDULE_DELAY, Some("I am not number")),
            (OTEL_BSP_EXPORT_TIMEOUT, Some("2046")),
        ];
        temp_env::with_vars(env_vars.clone(), || {
            let builder = BatchSpanProcessor::builder(
                InMemorySpanExporterBuilder::new().build(),
                runtime::Tokio,
            );
            // export batch size cannot exceed max queue size
            assert_eq!(builder.config.max_export_batch_size, 500);
            assert_eq!(
                builder.config.scheduled_delay,
                Duration::from_millis(OTEL_BSP_SCHEDULE_DELAY_DEFAULT)
            );
            assert_eq!(
                builder.config.max_queue_size,
                OTEL_BSP_MAX_QUEUE_SIZE_DEFAULT
            );
            assert_eq!(
                builder.config.max_export_timeout,
                Duration::from_millis(2046)
            );
        });

        env_vars.push((OTEL_BSP_MAX_QUEUE_SIZE, Some("120")));

        temp_env::with_vars(env_vars, || {
            let builder = BatchSpanProcessor::builder(
                InMemorySpanExporterBuilder::new().build(),
                runtime::Tokio,
            );
            assert_eq!(builder.config.max_export_batch_size, 120);
            assert_eq!(builder.config.max_queue_size, 120);
        });
    }

    #[tokio::test]
    async fn test_batch_span_processor() {
        let (exporter, mut export_receiver, _shutdown_receiver) = new_tokio_test_exporter();
        let config = BatchConfig {
            scheduled_delay: Duration::from_secs(60 * 60 * 24), // set the tick to 24 hours so we know the span must be exported via force_flush
            ..Default::default()
        };
        let processor =
            BatchSpanProcessor::new(Box::new(exporter), config, runtime::TokioCurrentThread);
        let handle = tokio::spawn(async move {
            loop {
                if let Some(span) = export_receiver.recv().await {
                    assert_eq!(span.span_context, new_test_export_span_data().span_context);
                    break;
                }
            }
        });
        tokio::time::sleep(Duration::from_secs(1)).await; // skip the first
        processor.on_end(new_test_export_span_data());
        let flush_res = processor.force_flush();
        assert!(flush_res.is_ok());
        let _shutdown_result = processor.shutdown();

        assert!(
            tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .is_ok(),
            "timed out in 5 seconds. force_flush may not export any data when called"
        );
    }

    struct BlockingExporter<D> {
        delay_for: Duration,
        delay_fn: D,
    }

    impl<D, DS> Debug for BlockingExporter<D>
    where
        D: Fn(Duration) -> DS + 'static + Send + Sync,
        DS: Future<Output = ()> + Send + Sync + 'static,
    {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("blocking exporter for testing")
        }
    }

    #[async_trait]
    impl<D, DS> SpanExporter for BlockingExporter<D>
    where
        D: Fn(Duration) -> DS + 'static + Send + Sync,
        DS: Future<Output = ()> + Send + Sync + 'static,
    {
        fn export(
            &mut self,
            _batch: Vec<SpanData>,
        ) -> futures_util::future::BoxFuture<'static, ExportResult> {
            use futures_util::FutureExt;
            Box::pin((self.delay_fn)(self.delay_for).map(|_| Ok(())))
        }
    }

    #[test]
    fn test_timeout_tokio_timeout() {
        // If time_out is true, then we ask exporter to block for 60s and set timeout to 5s.
        // If time_out is false, then we ask the exporter to block for 5s and set timeout to 60s.
        // Either way, the test should be finished within 5s.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(timeout_test_tokio(true));
    }

    #[test]
    fn test_timeout_tokio_not_timeout() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(timeout_test_tokio(false));
    }

    #[test]
    #[cfg(feature = "rt-async-std")]
    fn test_timeout_async_std_timeout() {
        async_std::task::block_on(timeout_test_std_async(true));
    }

    #[test]
    #[cfg(feature = "rt-async-std")]
    fn test_timeout_async_std_not_timeout() {
        async_std::task::block_on(timeout_test_std_async(false));
    }

    // If the time_out is true, then the result suppose to ended with timeout.
    // otherwise the exporter should be able to export within time out duration.
    #[cfg(feature = "rt-async-std")]
    async fn timeout_test_std_async(time_out: bool) {
        let config = BatchConfig {
            max_export_timeout: Duration::from_millis(if time_out { 5 } else { 60 }),
            scheduled_delay: Duration::from_secs(60 * 60 * 24), // set the tick to 24 hours so we know the span must be exported via force_flush
            ..Default::default()
        };
        let exporter = BlockingExporter {
            delay_for: Duration::from_millis(if !time_out { 5 } else { 60 }),
            delay_fn: async_std::task::sleep,
        };
        let processor = BatchSpanProcessor::new(Box::new(exporter), config, runtime::AsyncStd);
        processor.on_end(new_test_export_span_data());
        let flush_res = processor.force_flush();
        if time_out {
            assert!(flush_res.is_err());
        } else {
            assert!(flush_res.is_ok());
        }
        let shutdown_res = processor.shutdown();
        assert!(shutdown_res.is_ok());
    }

    // If the time_out is true, then the result suppose to ended with timeout.
    // otherwise the exporter should be able to export within time out duration.
    async fn timeout_test_tokio(time_out: bool) {
        let config = BatchConfig {
            max_export_timeout: Duration::from_millis(if time_out { 5 } else { 60 }),
            scheduled_delay: Duration::from_secs(60 * 60 * 24), // set the tick to 24 hours so we know the span must be exported via force_flush,
            ..Default::default()
        };
        let exporter = BlockingExporter {
            delay_for: Duration::from_millis(if !time_out { 5 } else { 60 }),
            delay_fn: tokio::time::sleep,
        };
        let processor =
            BatchSpanProcessor::new(Box::new(exporter), config, runtime::TokioCurrentThread);
        tokio::time::sleep(Duration::from_secs(1)).await; // skip the first
        processor.on_end(new_test_export_span_data());
        let flush_res = processor.force_flush();
        if time_out {
            assert!(flush_res.is_err());
        } else {
            assert!(flush_res.is_ok());
        }
        let shutdown_res = processor.shutdown();
        assert!(shutdown_res.is_ok());
    }
}
