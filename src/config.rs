use std::env;

pub const DEFAULT_MLX_URL: &str = "http://127.0.0.1:8000/v1/chat/completions";
pub const DEFAULT_MODEL_NAME: &str = "mlx-community/gemma-4-e4b-it-OptiQ-4bit";
// pub const DEFAULT_MODEL_NAME: &str = "mlx-community/gpt-oss-20b-MXFP4-Q4";
pub const STALE_MODEL_ALIASES: &[&str] = &["mlx", "mlx-community"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub mlx_url: String,
    pub mlx_model: String,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let mlx_url = env::var("MLX_URL").unwrap_or_else(|_| DEFAULT_MLX_URL.to_owned());
        let configured_model = env::var("MLX_MODEL").ok();
        let mlx_model = resolve_model_name(configured_model.as_deref());

        Self { mlx_url, mlx_model }
    }
}

pub fn resolve_model_name(configured: Option<&str>) -> String {
    let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) else {
        return DEFAULT_MODEL_NAME.to_owned();
    };

    if STALE_MODEL_ALIASES.contains(&configured) {
        return DEFAULT_MODEL_NAME.to_owned();
    }

    configured.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MODEL_NAME, resolve_model_name};

    #[test]
    fn falls_back_when_model_is_missing() {
        assert_eq!(resolve_model_name(None), DEFAULT_MODEL_NAME);
        assert_eq!(resolve_model_name(Some("")), DEFAULT_MODEL_NAME);
        assert_eq!(resolve_model_name(Some("   ")), DEFAULT_MODEL_NAME);
    }

    #[test]
    fn falls_back_for_stale_aliases() {
        assert_eq!(resolve_model_name(Some("mlx")), DEFAULT_MODEL_NAME);
        assert_eq!(
            resolve_model_name(Some("mlx-community")),
            DEFAULT_MODEL_NAME
        );
    }

    #[test]
    fn keeps_explicit_model_name() {
        assert_eq!(
            resolve_model_name(Some("mlx-community/gemma-4-e4b-it-OptiQ-4bit")),
            "mlx-community/gemma-4-e4b-it-OptiQ-4bit"
        );
    }
}
