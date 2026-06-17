use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;
use tracing_tree::HierarchicalLayer;

pub fn setup_tracing() {
    // NOTE: the underlying subscriber MUST be the Registry subscriber
    let subscriber = Registry::default()
        .with(LevelFilter::INFO)
        .with(
            HierarchicalLayer::new(2)
                .with_targets(true)
                .with_bracketed_fields(true),
        );

    tracing::subscriber::set_global_default(subscriber).unwrap();
}
