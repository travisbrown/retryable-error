//! Tools for attaching retry logic to error types.
use log::{log, Level};
use std::fmt::Debug;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tryhard::{
    backoff_strategies::BackoffStrategy, OnRetry, RetryFuture, RetryFutureConfig, RetryPolicy,
};

/// Execute a future with retries where the error type is `Retryable`.
pub fn retry_future<F, Fut, T, E>(f: F) -> RetryFuture<F, Fut, ErrorBackoff<E>, LogOnRetry>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: Retryable,
{
    tryhard::retry_fn(f).with_config(E::retry_config())
}

pub struct LogFuture {
    level: Option<Level>,
    message: Option<String>,
}

impl Future for LogFuture {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if let Some(level) = self.level {
            log!(
                level,
                "{}",
                self.message
                    .take()
                    .expect("LogFuture polled after completion")
            );
        }

        Poll::Ready(())
    }
}

pub struct LogOnRetry {
    level: Option<Level>,
}

impl<E: Debug> OnRetry<E> for LogOnRetry {
    type Future = LogFuture;

    fn on_retry(
        &mut self,
        attempts: u32,
        next_delay: Option<Duration>,
        previous_error: &E,
    ) -> Self::Future {
        match next_delay {
            Some(delay) => {
                let message = if self.level.is_none() {
                    None
                } else {
                    Some(format!(
                        "Retry {}; waiting {:?} after error: {:?}",
                        attempts, delay, previous_error
                    ))
                };
                LogFuture {
                    level: self.level,
                    message,
                }
            }
            None => LogFuture {
                level: None,
                message: None,
            },
        }
    }
}

pub struct ErrorBackoff<E>
where
    E: ?Sized,
{
    delay: Duration,
    _error: PhantomData<E>,
}

impl<'a, E: Retryable> BackoffStrategy<'a, E> for ErrorBackoff<E> {
    type Output = RetryPolicy;

    fn delay(&mut self, _attempt: u32, error: &'a E) -> RetryPolicy {
        error.custom_retry_policy().unwrap_or_else(|| {
            let prev_delay = self.delay;
            self.delay *= 2;
            RetryPolicy::Delay(prev_delay)
        })
    }
}

/// The `Retryable` trait allows an error type to define retry logic for
/// specific errors.
pub trait Retryable {
    /// Return the maximum number of retries.
    fn max_retries() -> u32;

    /// Return the default initial delay.
    fn default_initial_delay() -> Duration;

    /// Return the log level for this error type (an empty value indicates that
    /// no logging will be done).
    fn log_level() -> Option<Level>;

    /// Return a retry policy for the given error value.
    ///
    /// An empty value represents the default.
    fn custom_retry_policy(&self) -> Option<RetryPolicy>;

    /// Generate a new backoff strategy instance.
    fn new_backoff() -> ErrorBackoff<Self> {
        ErrorBackoff {
            delay: Self::default_initial_delay(),
            _error: PhantomData,
        }
    }

    /// Generate a new retry configuration instance.
    fn retry_config() -> RetryFutureConfig<ErrorBackoff<Self>, LogOnRetry> {
        RetryFutureConfig::new(Self::max_retries())
            .on_retry(LogOnRetry {
                level: Self::log_level(),
            })
            .custom_backoff(Self::new_backoff())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    #[derive(thiserror::Error, Debug, Eq, PartialEq)]
    pub enum Error {
        #[error("Error A")]
        A(String),
        #[error("Error B")]
        B(u64),
    }

    impl Retryable for Error {
        fn max_retries() -> u32 {
            7
        }

        fn log_level() -> Option<log::Level> {
            None
        }

        fn default_initial_delay() -> Duration {
            Duration::from_millis(250)
        }

        fn custom_retry_policy(&self) -> Option<RetryPolicy> {
            match self {
                Error::A(_) => Some(RetryPolicy::Delay(Duration::from_millis(100))),
                Error::B(_) => Some(RetryPolicy::Break),
            }
        }
    }

    #[tokio::test]
    async fn success() {
        let counter = Mutex::new(0);

        let future = || async {
            let mut c = counter.lock().await;
            *c += 1;

            if *c < 5 {
                Err(Error::A("baz".to_string()))
            } else {
                Ok("foo")
            }
        };

        assert_eq!(retry_future(future).await.unwrap(), "foo");
    }

    #[tokio::test]
    async fn failure() {
        let counter = Mutex::new(0);

        let future = || async {
            let mut c = counter.lock().await;
            *c += 1;

            let result: Result<&'static str, Error> = if *c < 5 {
                Err(Error::A("baz".to_string()))
            } else {
                Err(Error::B(123))
            };

            result
        };

        assert_eq!(retry_future(future).await, Err(Error::B(123)));
    }
}
