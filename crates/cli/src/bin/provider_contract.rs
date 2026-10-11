use anyhow::Result;
use clap::Parser;
use executor::{HttpProvider, Runtime};
use exo_managed_agents::http::RuntimeClient;
use exoharness::AgentId;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    url: String,
    #[arg(long, env = "EXO_TEST_API_KEY", hide_env_values = true)]
    token: String,
    #[arg(long)]
    agent: AgentId,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = RuntimeClient::new(&args.url)?.with_bearer_token(args.token);
    let runtime = Runtime::new(HttpProvider::new(client), None);
    let result = executor::contract_tests::test_provider(&runtime, args.agent, || {}).await;
    runtime.shutdown().await?;
    result
}
