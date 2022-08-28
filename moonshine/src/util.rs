use tokio::task::JoinHandle;

pub async fn flatten<T>(handle: JoinHandle<Result<T, ()>>) -> Result<T, ()> {
	match handle.await {
		Ok(Ok(result)) => Ok(result),
		Ok(Err(err)) => Err(err),
		Err(err) => Err(()),
	}
}
