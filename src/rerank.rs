use crate::{config::Reranker, store::Hit};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::json;
use std::{fs, time::Duration};

pub const CANDIDATES: usize = 30;

#[derive(Deserialize)]
struct Response {
    results: Vec<Score>,
}

#[derive(Deserialize)]
struct Score {
    index: usize,
    relevance_score: f64,
}

pub fn rank(client: &Client, config: &Reranker, query: &str, hits: &[Hit]) -> Result<Vec<usize>> {
    let documents: Vec<_> = hits
        .iter()
        .map(|hit| {
            let title: String = hit.title.chars().take(160).collect();
            let text: String = hit.snippet.chars().take(900).collect();
            format!("{title}\n{text}")
        })
        .collect();
    let url = format!("{}/v1/rerank", config.url.trim_end_matches('/'));
    let mut request = client
        .post(url)
        .timeout(Duration::from_secs(12))
        .json(&json!({"query": query, "documents": documents}));
    if let Some(path) = &config.api_key_file {
        let secret = fs::read_to_string(path).context("Read local reranker API key")?;
        request = request.bearer_auth(secret.trim());
    }
    let mut response: Response = request
        .send()
        .context("Connect to the local reranker")?
        .error_for_status()
        .context("Local reranker rejected the request")?
        .json()
        .context("Read local reranker scores")?;
    let mut seen = vec![false; hits.len()];
    ensure!(
        response.results.len() == seen.len(),
        "Reranker returned incomplete scores"
    );
    for score in &response.results {
        ensure!(
            score.index < seen.len()
                && score.relevance_score.is_finite()
                && !std::mem::replace(&mut seen[score.index], true),
            "Reranker returned invalid scores"
        );
    }
    response.results.sort_by(|left, right| {
        right
            .relevance_score
            .total_cmp(&left.relevance_score)
            .then_with(|| left.index.cmp(&right.index))
    });
    let mut positions = vec![0; hits.len()];
    for (position, score) in response.results.into_iter().enumerate() {
        positions[score.index] = position;
    }
    Ok(positions)
}
