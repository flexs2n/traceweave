//! Lexical + symbol search (FR-006, TIS §13.4).

use crate::{meta_for, output, resolve_repo, AppContext, OpOutput};
use ccc_domain::CccError;
use ccc_search::{FtsSearchBackend, SearchBackend, SearchRequest};

pub async fn search(
    ctx: &AppContext,
    repo_path: Option<&std::path::Path>,
    query: &str,
    limit: u32,
) -> Result<OpOutput, CccError> {
    let store = &ctx.store;
    let repo = resolve_repo(store, repo_path).await?;
    let backend = FtsSearchBackend::new(&**store, &**store, &**store);
    let response = backend
        .search(SearchRequest {
            repository_id: repo.repository_id,
            query: query.to_string(),
            snapshot_id: None,
            limit,
        })
        .await?;
    let mut human = String::new();
    if response.hits.is_empty() {
        human.push_str(&format!("No results for {query:?}.\n"));
    }
    for hit in &response.hits {
        human.push_str(&format!(
            "[{:?}] {:<8} {:?} ({})\n",
            hit.match_class,
            hit.kind,
            hit.title.as_deref().unwrap_or(&hit.path),
            hit.path
        ));
    }
    output(
        &response,
        meta_for(
            &repo,
            Some(response.snapshot_id),
            Some(response.generation_id),
        ),
        human,
    )
}
