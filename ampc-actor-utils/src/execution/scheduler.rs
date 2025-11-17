use eyre::Result;
use futures::future::JoinAll;
use std::future::Future;
use tokio::task::JoinError;

pub async fn parallelize<F, T>(tasks: impl Iterator<Item = F>) -> Result<Vec<T>>
where
    F: Future<Output = Result<T>> + Send + 'static,
    F::Output: Send + 'static,
{
    tasks
        .map(tokio::spawn)
        .collect::<JoinAll<_>>()
        .await
        .into_iter()
        .collect::<Result<Result<Vec<T>>, JoinError>>()?
}
