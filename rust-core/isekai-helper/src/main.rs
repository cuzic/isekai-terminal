#[tokio::main]
async fn main() -> anyhow::Result<()> {
    isekai_helper::run_from_args(std::env::args().skip(1)).await
}
