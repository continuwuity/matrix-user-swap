use std::{
    any::TypeId,
    collections::HashMap,
    sync::{Arc, RwLock},
    time::SystemTime,
};

use ruma::api::{
    OutgoingRequest,
    client::error::{Error as ClientError, ErrorKind, RetryAfter},
};
use tokio::{
    sync::Mutex as AsyncMutex,
    time::{Duration, Instant},
};
use tracing as t;

use crate::{Client, RumaError};

/// Default period to wait between non-rate-limited requests.
const DEFAULT_REQUEST_PERIOD: Duration = Duration::from_millis(200);

/// Default period to wait before retrying a rate limit request, if the server
/// does not specify the `retry_after` field.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);

#[derive(Default, Debug)]
struct LimitState {
    /// Time that we are allowed to make the next request to this endpoint.
    ///
    /// We only allow one concurrent request of each type, to avoid a
    /// "thundering herd" effect when the rate limit period elapses. In order
    /// to make a request, a task needs to lock the mutex then wait until
    /// the specified time.
    next_request: AsyncMutex<Option<Instant>>,
}

#[derive(Clone, Debug)]
pub(crate) struct RateLimitedClient {
    inner: Client,
    /// Independent rate limiting state for each endpoint
    ///
    /// We track state for each endpoint independly instead of tracking one
    /// global state because synapse implements separate rate limiting logic
    /// for different endpoints. It is possible to be limited on one endpoint
    /// but still able to send requests on others.
    ///
    /// Endpoints are distinguished by their ruma request type.
    state: Arc<RwLock<HashMap<TypeId, Arc<LimitState>>>>,
}

impl RateLimitedClient {
    pub(crate) fn new(inner: Client) -> RateLimitedClient {
        RateLimitedClient {
            inner,
            state: Default::default(),
        }
    }

    fn request_state(&self, id: TypeId) -> Arc<LimitState> {
        {
            // We don't care about lock poisoning, just propagate it
            let states = self.state.read().unwrap();
            if let Some(state) = states.get(&id) {
                return Arc::clone(state);
            }
        }

        let state = Arc::new(LimitState::default());
        let mut states = self.state.write().unwrap();
        // It's possible that another thread has already written state for
        // this request type in between dropping the read lock and acquiring
        // the write lock. In this case, use the other thread's state rather
        // than the one we just created, since it may already be in use.
        Arc::clone(states.entry(id).or_insert(state))
    }

    pub(crate) async fn send_request<R>(
        &self,
        request: R,
    ) -> Result<R::IncomingResponse, RumaError>
    where
        R: OutgoingRequest<EndpointError = ClientError> + Clone + 'static,
    {
        let state = self.request_state(TypeId::of::<R>());

        let mut next_request = state.next_request.lock().await;
        loop {
            if let Some(next_request) = *next_request {
                tokio::time::sleep_until(next_request).await;
            }

            let response = self.inner.send_request(request.clone()).await;
            let now = Instant::now();
            if let Err(error) = &response
                && let Some(ErrorKind::LimitExceeded {
                    retry_after,
                }) = error.error_kind()
            {
                t::debug!(
                    ?retry_after,
                    "Request was rate limited, retrying after a delay"
                );
                *next_request = Some(match retry_after {
                    Some(RetryAfter::Delay(delay)) => now + *delay,
                    Some(RetryAfter::DateTime(time)) => {
                        // TODO: is there a simpler way to do this conversion?
                        match SystemTime::now().duration_since(*time) {
                            // Time is in the future
                            Ok(delay) => now + delay,
                            // Time is in the past
                            Err(error) => now - error.duration(),
                        }
                    }
                    None => now + DEFAULT_RETRY_AFTER,
                });
            } else {
                // We were not rate limited
                *next_request = Some(now + DEFAULT_REQUEST_PERIOD);
                return response;
            }
        }
    }
}
