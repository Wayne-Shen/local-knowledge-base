use crate::{
    Runtime,
    config::{Config, MAX_QUERY_BYTES},
    rerank,
    store::{Hit, IndexJob, Store},
    worker,
};
use anyhow::{Context, Result, ensure};
use jieba_rs::Jieba;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::LazyLock,
};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term,
    collector::TopDocs,
    doc,
    query::{BooleanQuery, BoostQuery, Occur, PhraseQuery, Query, TermQuery},
    schema::{
        Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
        Value,
    },
};

static JIEBA: LazyLock<Jieba> = LazyLock::new(Jieba::new);
pub const INDEX_DIR: &str = "tantivy-v2";

pub struct FullText {
    index: Index,
    reader: IndexReader,
    id: Field,
    title: Field,
    body: Field,
    phrase: Field,
    filename: Field,
}

#[derive(Default)]
struct Lexical {
    ranked: Vec<i64>,
    phrases: BTreeSet<i64>,
    filenames: BTreeSet<i64>,
}

impl Lexical {
    fn priority(&self, id: i64) -> u8 {
        if self.filenames.contains(&id) {
            2
        } else {
            u8::from(self.phrases.contains(&id))
        }
    }
}

impl FullText {
    pub fn open(path: &Path) -> Result<(Self, bool)> {
        fs::create_dir_all(path)?;
        let mut schema = Schema::builder();
        let index_options = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("whitespace")
                .set_index_option(IndexRecordOption::WithFreqs),
        );
        let id = schema.add_i64_field("chunk_id", INDEXED | STORED);
        let title = schema.add_text_field("title", index_options.clone());
        let body = schema.add_text_field("body", index_options);
        let phrase = schema.add_text_field(
            "phrase",
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("whitespace")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
        );
        let filename = schema.add_text_field("filename", STRING);
        let new = !path.join("meta.json").is_file();
        let index = if new {
            Index::create_in_dir(path, schema.build())?
        } else {
            let index = Index::open_in_dir(path)?;
            ensure!(
                index.schema() == schema.build(),
                "Unexpected Tantivy index schema"
            );
            index
        };
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok((
            Self {
                index,
                reader,
                id,
                title,
                body,
                phrase,
                filename,
            },
            new,
        ))
    }

    pub fn writer(&self) -> Result<IndexWriter<TantivyDocument>> {
        Ok(self.index.writer_with_num_threads(1, 24_000_000)?)
    }

    pub fn update(
        &self,
        writer: &mut IndexWriter<TantivyDocument>,
        jobs: &[IndexJob],
    ) -> Result<()> {
        for job in jobs {
            writer.delete_term(Term::from_field_i64(self.id, job.id));
            let mut document = doc!(
                self.id => job.id,
                self.title => tokenize(&job.filenames.join(" ")).join(" "),
                self.body => tokenize(&job.text).join(" "),
                self.phrase => phrase_tokens(&job.text).join(" "),
            );
            let mut names = BTreeSet::new();
            for filename in &job.filenames {
                names.insert(fold(filename));
                if let Some(stem) = Path::new(filename).file_stem().and_then(|v| v.to_str()) {
                    names.insert(fold(stem));
                }
            }
            for name in names {
                document.add_text(self.filename, &name);
            }
            writer.add_document(document)?;
        }
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    fn search(&self, query: &str, limit: usize) -> Result<Lexical> {
        let terms: BTreeSet<_> = tokenize(query).into_iter().collect();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for token in terms.into_iter().take(100) {
            let body = TermQuery::new(
                Term::from_field_text(self.body, &token),
                IndexRecordOption::WithFreqs,
            );
            let title = TermQuery::new(
                Term::from_field_text(self.title, &token),
                IndexRecordOption::WithFreqs,
            );
            clauses.push((Occur::Should, Box::new(body)));
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(title), 2.2)),
            ));
        }
        let searcher = self.reader.searcher();
        let phrase_queries: Vec<(Occur, Box<dyn Query>)> = phrases(query)
            .into_iter()
            .map(|tokens| {
                let terms = tokens
                    .iter()
                    .map(|token| Term::from_field_text(self.phrase, token))
                    .collect();
                (
                    Occur::Should,
                    Box::new(PhraseQuery::new(terms)) as Box<dyn Query>,
                )
            })
            .collect();
        let name = query.trim().trim_matches(['"', '“', '”']).trim();
        let exact_name = TermQuery::new(
            Term::from_field_text(self.filename, &fold(name)),
            IndexRecordOption::Basic,
        );
        let ranked = if clauses.is_empty() {
            Vec::new()
        } else {
            self.top_ids(&searcher, &BooleanQuery::new(clauses), limit)?
        };
        let phrases = if phrase_queries.is_empty() {
            BTreeSet::new()
        } else {
            self.top_ids(&searcher, &BooleanQuery::new(phrase_queries), limit)?
                .into_iter()
                .collect()
        };
        let filenames = self
            .top_ids(&searcher, &exact_name, limit)?
            .into_iter()
            .collect();
        Ok(Lexical {
            ranked,
            phrases,
            filenames,
        })
    }

    fn top_ids(
        &self,
        searcher: &tantivy::Searcher,
        query: &dyn Query,
        limit: usize,
    ) -> Result<Vec<i64>> {
        let top = searcher.search(query, &TopDocs::with_limit(limit))?;
        top.into_iter()
            .map(|(_, address)| {
                let document: TantivyDocument = searcher.doc(address)?;
                document
                    .get_first(self.id)
                    .and_then(|value| value.as_i64())
                    .context("Missing indexed chunk ID")
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_exact_filenames_and_contiguous_phrases() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (index, _) = FullText::open(directory.path())?;
        let mut writer = index.writer()?;
        let jobs = [
            IndexJob {
                id: 1,
                filenames: vec!["Meal Plan.pdf".into(), "周末菜单.pdf".into()],
                text: "The menu includes tomato tofu with shrimp. 今天晚餐吃番茄豆腐虾仁饭。"
                    .into(),
            },
            IndexJob {
                id: 2,
                filenames: vec!["Menu Notes.md".into()],
                text: "Tofu, tomato, shrimp, tofu, tomato. 豆腐很好吃，番茄配虾仁。".into(),
            },
        ];
        index.update(&mut writer, &jobs)?;
        for filename in ["MEAL PLAN.PDF", "Meal Plan", "周末菜单.pdf", "周末菜单"] {
            let matches = index.search(filename, 10)?;
            assert_eq!(matches.priority(1), 2, "{filename}");
            assert_eq!(matches.priority(2), 0, "{filename}");
        }
        for phrase in ["Tomato tofu", "a \"tomato tofu\" recipe", "番茄豆腐虾仁"] {
            let matches = index.search(phrase, 10)?;
            assert_eq!(matches.priority(1), 1, "{phrase}");
            assert_eq!(matches.priority(2), 0, "{phrase}");
        }
        Ok(())
    }
}

fn tokenize(text: &str) -> Vec<String> {
    let normalized = fold(text);
    JIEBA
        .cut_for_search(&normalized, true)
        .into_iter()
        .flat_map(|token| token.split(|character: char| !character.is_alphanumeric()))
        .filter(|word| !word.is_empty() && word.len() <= 256)
        .map(str::to_owned)
        .collect()
}

fn fold(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            match character {
                '！'..='～' => char::from_u32(u32::from(character) - 0xfee0).unwrap(),
                '　' => ' ',
                _ => character,
            }
            .to_lowercase()
        })
        .collect()
}

fn phrase_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    for ch in fold(text).chars() {
        // Use individual Han and kana characters so phrase matches do not depend on Jieba's context.
        let cjk = matches!(ch,
            '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}' |
            '\u{f900}'..='\u{faff}' | '\u{20000}'..='\u{323af}'
        );
        if cjk || !ch.is_alphanumeric() {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            if cjk {
                tokens.push(ch.to_string());
            }
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    tokens
}

fn phrases(query: &str) -> BTreeSet<Vec<String>> {
    let quotes = query.replace(['“', '”'], "\"");
    std::iter::once(query)
        .chain(quotes.split('"').skip(1).step_by(2).take(4))
        .map(phrase_tokens)
        .filter(|tokens| {
            (2..=80).contains(&tokens.len()) && tokens.iter().all(|token| token.len() <= 256)
        })
        .collect()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub keyword_only: bool,
}

fn default_limit() -> usize {
    10
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.query.trim().is_empty() && self.query.len() <= MAX_QUERY_BYTES,
            "Enter a query between 1 and {MAX_QUERY_BYTES} UTF-8 bytes"
        );
        ensure!(
            (1..=30).contains(&self.limit),
            "Limit must be between 1 and 30"
        );
        Ok(())
    }
}

#[derive(Serialize)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub mode: &'static str,
    pub warning: Option<String>,
    pub reranked: bool,
}

pub fn retrieve(
    runtime: &Runtime,
    config: &Config,
    store: &Store,
    request: &Request,
) -> Result<Results> {
    let query = request.query.trim();
    let candidate_limit = (request.limit * 4).max(rerank::CANDIDATES);
    let text = runtime.full_text.search(query, candidate_limit)?;
    let mut warning = None;
    let mut mode = "keyword";
    let mut semantic = Vec::new();
    if !request.keyword_only {
        let _vectors = runtime.vectors.read().unwrap();
        if store.has_vectors()? {
            match worker::search(config, query, candidate_limit) {
                Ok(rows) => {
                    semantic = rows;
                    mode = "hybrid";
                }
                Err(error) => {
                    warning = Some(format!("Semantic search unavailable. {error:#}"));
                }
            }
        }
    }

    let mut ranks = BTreeMap::<i64, f64>::new();
    for ids in [&text.ranked, &semantic] {
        for (position, id) in ids.iter().enumerate() {
            *ranks.entry(*id).or_default() += 1.0 / (60.0 + position as f64);
        }
    }
    for id in text.filenames.iter().chain(&text.phrases) {
        ranks.entry(*id).or_default();
    }
    let mut ranks: Vec<_> = ranks.into_iter().collect();
    ranks.sort_by(|left, right| {
        text.priority(right.0)
            .cmp(&text.priority(left.0))
            .then_with(|| right.1.total_cmp(&left.1))
            .then_with(|| left.0.cmp(&right.0))
    });
    let reranker = config
        .models
        .reranker
        .as_ref()
        .filter(|_| !request.keyword_only);
    let limit = if reranker.is_some() {
        rerank::CANDIDATES
    } else {
        request.limit
    };
    let mut hits = Vec::new();
    for (id, _) in ranks {
        if let Some(hit) = store.hit(id)? {
            hits.push(hit);
            if hits.len() == limit {
                break;
            }
        }
    }
    let mut reranked = false;
    if let Some(reranker) = reranker
        && hits.len() > 1
    {
        match rerank::rank(&runtime.http, reranker, query, &hits) {
            Ok(positions) => {
                let mut ranked: Vec<_> = hits.into_iter().enumerate().collect();
                ranked.sort_by(|(left, a), (right, b)| {
                    let score = |index: usize| {
                        0.7 / (15.0 + positions[index] as f64) + 0.3 / (15.0 + index as f64)
                    };
                    text.priority(b.id)
                        .cmp(&text.priority(a.id))
                        .then_with(|| score(*right).total_cmp(&score(*left)))
                        .then_with(|| left.cmp(right))
                });
                hits = ranked.into_iter().map(|(_, hit)| hit).collect();
                reranked = true;
            }
            Err(error) => {
                tracing::warn!("Local reranker: {error:#}");
                let note = "Local reranking unavailable. Showing the original search ranking.";
                warning = Some(
                    warning
                        .map_or_else(|| note.to_owned(), |previous| format!("{previous} {note}")),
                );
            }
        }
    }
    hits.truncate(request.limit);
    Ok(Results {
        hits,
        mode,
        warning,
        reranked,
    })
}
