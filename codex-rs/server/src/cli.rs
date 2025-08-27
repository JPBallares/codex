use clap::Parser;

#[derive(Debug, Clone, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ApiMode {
    Openai,
    Mcp,
    Both,
}

#[derive(Parser, Debug, Clone)]
#[command(version)]
pub struct Cli {
    /// Host interface to bind. Defaults to 127.0.0.1.
    #[arg(long = "host", default_value = "127.0.0.1")]
    pub host: String,

    /// Port to listen on. Defaults to 8765.
    #[arg(long = "port", short = 'p', default_value_t = 8765)]
    pub port: u16,

    /// Static bearer token for protecting the local API.
    #[arg(long = "token")]
    pub token: Option<String>,

    /// Disable auth for localhost development only.
    #[arg(long = "no-auth", default_value_t = false)]
    pub no_auth: bool,

    /// Allowed CORS origins (repeatable).
    #[arg(long = "cors-origin", value_name = "ORIGIN", num_args = 0..)]
    pub cors_origins: Vec<String>,

    /// Which API surfaces to enable.
    #[arg(long = "api", value_enum, default_value_t = ApiMode::Openai)]
    pub api: ApiMode,
}
