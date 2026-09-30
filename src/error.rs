use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// Error de la API. Se responde con su código HTTP y el mensaje en texto plano.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// Error inesperado del servidor: se registra en stderr y se responde 500.
    pub fn internal(err: impl std::fmt::Display) -> Self {
        eprintln!("Error interno: {err}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
