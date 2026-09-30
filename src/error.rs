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

    /// Error inesperado del servidor: el detalle (rutas, mensajes de pánico…)
    /// sólo va a stderr; al cliente, que puede acabar mostrándolo a usuarios
    /// finales, se le responde 500 con un mensaje genérico.
    pub fn internal(err: impl std::fmt::Display) -> Self {
        eprintln!("Error interno: {err}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "error interno del servidor".to_string(),
        }
    }

    /// Indica a qué elemento de un lote se refiere el error, con su índice en
    /// el array (desde 0), como en JSON: "[17]: …".
    pub fn for_item(self, index: usize) -> Self {
        Self {
            message: format!("[{index}]: {}", self.message),
            ..self
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
