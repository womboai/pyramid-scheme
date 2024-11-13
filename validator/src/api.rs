use crate::validator::{CENTER_COLUMN_FILE, STATE_DATA_FILE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use axum_extra::headers::Range;
use axum_extra::TypedHeader;
use axum_range::{KnownSize, Ranged};
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::fs::File as SyncFile;
use std::io;
use thiserror::Error;
use tokio::fs::File;

#[derive(Deserialize, Serialize)]
struct MinimalState {
    step: u64,
}

#[derive(Error, Debug)]
#[error(transparent)]
pub struct ErrorWrapper<E>(#[from] E);

#[derive(Error, Debug)]
pub enum StepError {
    #[error(transparent)]
    ParseError(#[from] serde_json::Error),

    #[error(transparent)]
    IoError(#[from] io::Error),
}

impl<E> IntoResponse for ErrorWrapper<E>
where
    E: Display,
{
    fn into_response(self) -> Response {
        let mut response = self.0.to_string().into_response();

        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;

        response
    }
}

pub(crate) async fn current_step() -> Result<Json<u64>, ErrorWrapper<StepError>> {
    let file = SyncFile::open(STATE_DATA_FILE).map_err(StepError::IoError)?;
    let state = serde_json::from_reader::<_, MinimalState>(&file).map_err(StepError::ParseError)?;

    Ok(Json::from(state.step))
}

pub(crate) async fn last_n_bits(
    TypedHeader(range): TypedHeader<Range>,
) -> Result<Ranged<KnownSize<File>>, ErrorWrapper<io::Error>> {
    let file = File::open(CENTER_COLUMN_FILE).await?;
    let body = KnownSize::file(file).await?;

    Ok(Ranged::new(Some(range), body))
}
