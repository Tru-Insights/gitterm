//! Plans/docs adapter: share the app's live root handles with core routes.
pub use gitterm_core::plans::DocumentSource;

pub fn routes(
    state: crate::log_server::ServerState,
) -> warp::filters::BoxedFilter<(warp::reply::Response,)> {
    gitterm_core::plans::routes(gitterm_core::plans::DocumentRoots {
        plans_dir: state.plans_dir.clone(),
        docs_dir: state.docs_dir.clone(),
    })
}
