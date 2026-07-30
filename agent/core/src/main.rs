use pca_domain::AgentStatus;
use pca_provider_contracts::ProviderStatus;

fn main() {
    let agent = AgentStatus::Initializing;
    let wechat = ProviderStatus::WaitingSource;
    println!("pca-agentd scaffold: agent={agent:?}, wechat={wechat:?}");
}
