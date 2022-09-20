//! A module providing the `JsonRpcErrorCount` metric.

use ethers::providers::ProviderError;
use metered::metric::{Advice, Enter, OnResult};
use metered::{
    atomic::AtomicInt,
    clear::Clear,
    metric::{Counter, Metric},
};
use serde::Serialize;
use std::ops::Deref;

/// A metric counting how many times an expression typed std `Result` as
/// returned an `Err` variant.
///
/// This is a light-weight metric.
///
/// By default, `ErrorCount` uses a lock-free `u64` `Counter`, which makes sense
/// in multithread scenarios. Non-threaded applications can gain performance by
/// using a `std::cell:Cell<u64>` instead.
#[derive(Clone, Default, Debug, Serialize)]
pub struct JsonRpcErrorCount<C: Counter = AtomicInt<u64>>(pub C);

impl<C: Counter, T> Metric<Result<T, ProviderError>> for JsonRpcErrorCount<C> {}

impl<C: Counter> Enter for JsonRpcErrorCount<C> {
    type E = ();
    fn enter(&self) {}
}

impl<C: Counter, T> OnResult<Result<T, ProviderError>> for JsonRpcErrorCount<C> {
    /// Unlike the default ErrorCount, this one does not increment for internal jsonrpc errors
    /// TODO: count errors like this on another helper
    fn on_result(&self, _: (), r: &Result<T, ProviderError>) -> Advice {
        match r {
            Ok(_) => {}
            Err(ProviderError::JsonRpcClientError(_)) => {
                self.0.incr();
            }
            Err(_) => {
                // TODO: count jsonrpc errors
            }
        }
        Advice::Return
    }
}

impl<C: Counter> Clear for JsonRpcErrorCount<C> {
    fn clear(&self) {
        self.0.clear()
    }
}

impl<C: Counter> Deref for JsonRpcErrorCount<C> {
    type Target = C;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
