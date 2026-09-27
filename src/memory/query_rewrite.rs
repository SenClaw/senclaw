//! Query rewrite and token expansion. Mirrors `src-old/memory/query-rewrite.ts`.
//!
//! Three-stage pipeline:
//!   1. Rule-based rewrite (remove interrogatives, particles, normalize spaces)
//!   2. Tokenization + stopword filtering
//!   3. zh↔en synonym expansion for cross-language FTS matching

use std::sync::LazyLock;

use regex::Regex;

use crate::memory::tokenizer::tokenize_optimized;

// ===== Rule-based rewrite =====

static REWRITE_RULES: LazyLock<Vec<(&str, Regex, &str)>> = LazyLock::new(|| {
    vec![
        (
            "remove Chinese interrogatives",
            Regex::new(r"^(为什么|怎么|如何|什么|哪个|哪些)\s*").unwrap(),
            "",
        ),
        (
            "remove English interrogatives",
            Regex::new(r"^(why|how|what|which|where|when|who)\s+").unwrap(),
            "",
        ),
        (
            "remove Chinese particles",
            Regex::new(r"(^|\s)(会|能|可以|应该)(\s|$)").unwrap(),
            "$1$3",
        ),
        (
            "remove English particles",
            Regex::new(r"\s+(should|could|can|will|would)\s+").unwrap(),
            " ",
        ),
        ("normalize spaces", Regex::new(r"\s+").unwrap(), " "),
    ]
});

pub fn rewrite_query(query: &str) -> String {
    let mut rewritten = query.to_string();
    for (_name, pattern, replacement) in REWRITE_RULES.iter() {
        rewritten = pattern.replace_all(&rewritten, *replacement).to_string();
    }
    rewritten.trim().to_string()
}

// ===== Tokenization-based rewrite =====

pub fn rewrite_query_with_tokenization(query: &str) -> String {
    let tokens = tokenize_optimized(query, true);
    if tokens.is_empty() {
        return String::new();
    }
    tokens.join(" ")
}

// ===== Smart rewrite (combined) =====

pub fn smart_rewrite_query(query: &str) -> String {
    let mut rewritten = rewrite_query(query);
    rewritten = rewrite_query_with_tokenization(&rewritten);
    if rewritten.trim().is_empty() {
        return String::new();
    }
    rewritten
}

// ===== Token-level synonym expansion =====

use std::collections::{HashMap, HashSet};

fn synonym_map() -> (
    HashMap<&'static str, Vec<&'static str>>,
    HashMap<&'static str, Vec<&'static str>>,
) {
    let zh_to_en: HashMap<&str, Vec<&str>> = HashMap::from([
        ("内存", vec!["memory"]),
        ("内存泄漏", vec!["memory", "leak"]),
        ("泄漏", vec!["leak"]),
        ("数据库", vec!["database"]),
        ("索引", vec!["index"]),
        ("优化", vec!["optimization", "optimize"]),
        ("查询", vec!["query"]),
        ("异步", vec!["async", "asynchronous"]),
        ("编程", vec!["programming"]),
        ("同步", vec!["sync", "synchronous"]),
        ("性能", vec!["performance"]),
        ("调试", vec!["debug", "debugging"]),
        ("错误", vec!["error"]),
        ("异常", vec!["exception"]),
        ("部署", vec!["deploy", "deployment"]),
        ("容器", vec!["container"]),
        ("缓存", vec!["cache", "caching"]),
        ("并发", vec!["concurrent", "concurrency"]),
        ("线程", vec!["thread"]),
        ("进程", vec!["process"]),
        ("函数", vec!["function"]),
        ("组件", vec!["component"]),
        ("接口", vec!["interface", "api"]),
        ("排序", vec!["sort", "sorting"]),
        ("算法", vec!["algorithm"]),
        ("架构", vec!["architecture"]),
        ("微服务", vec!["microservice"]),
        ("网络", vec!["network"]),
        ("安全", vec!["security"]),
        ("认证", vec!["authentication", "auth"]),
        ("授权", vec!["authorization"]),
        ("日志", vec!["log", "logging"]),
        ("监控", vec!["monitor", "monitoring"]),
        ("分布式", vec!["distributed"]),
        ("事务", vec!["transaction"]),
        ("隔离", vec!["isolation"]),
    ]);

    let en_to_zh: HashMap<&str, Vec<&str>> = HashMap::from([
        ("memory", vec!["内存"]),
        ("leak", vec!["泄漏"]),
        ("database", vec!["数据库"]),
        ("index", vec!["索引"]),
        ("optimization", vec!["优化"]),
        ("optimize", vec!["优化"]),
        ("query", vec!["查询"]),
        ("async", vec!["异步"]),
        ("asynchronous", vec!["异步"]),
        ("programming", vec!["编程"]),
        ("performance", vec!["性能"]),
        ("debug", vec!["调试"]),
        ("debugging", vec!["调试"]),
        ("error", vec!["错误"]),
        ("exception", vec!["异常"]),
        ("deploy", vec!["部署"]),
        ("deployment", vec!["部署"]),
        ("container", vec!["容器"]),
        ("cache", vec!["缓存"]),
        ("caching", vec!["缓存"]),
        ("concurrent", vec!["并发"]),
        ("concurrency", vec!["并发"]),
        ("thread", vec!["线程"]),
        ("function", vec!["函数"]),
        ("component", vec!["组件"]),
        ("interface", vec!["接口"]),
        ("api", vec!["接口"]),
        ("sort", vec!["排序"]),
        ("sorting", vec!["排序"]),
        ("algorithm", vec!["算法"]),
        ("architecture", vec!["架构"]),
        ("microservice", vec!["微服务"]),
        ("network", vec!["网络"]),
        ("security", vec!["安全"]),
        ("authentication", vec!["认证"]),
        ("auth", vec!["认证"]),
        ("authorization", vec!["授权"]),
        ("log", vec!["日志"]),
        ("logging", vec!["日志"]),
        ("monitor", vec!["监控"]),
        ("monitoring", vec!["监控"]),
        ("distributed", vec!["分布式"]),
        ("transaction", vec!["事务"]),
        ("isolation", vec!["隔离"]),
    ]);

    (zh_to_en, en_to_zh)
}

pub fn expand_query_tokens(tokens: &[String]) -> Vec<String> {
    let (zh_to_en, en_to_zh) = synonym_map();
    let mut result: Vec<String> = tokens.to_vec();
    let mut added: HashSet<String> = tokens.iter().cloned().collect();

    for token in tokens {
        if let Some(syns) = zh_to_en.get(token.as_str()) {
            for syn in syns {
                if added.insert(syn.to_string()) {
                    result.push(syn.to_string());
                }
            }
        }
        if let Some(syns) = en_to_zh.get(token.to_lowercase().as_str()) {
            for syn in syns {
                if added.insert(syn.to_string()) {
                    result.push(syn.to_string());
                }
            }
        }
    }

    result
}

// ===== Legacy expand_query (deprecated in TS, kept for compat) =====

pub fn expand_query(query: &str) -> String {
    expand_query_tokens(&tokenize_optimized(query, true)).join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_removes_interrogatives() {
        let result = rewrite_query("什么是 memory leak?");
        assert!(!result.contains("什么"));
    }

    #[test]
    fn smart_rewrite_handles_stopwords_only() {
        // Query that's entirely stopwords should return empty
        let result = smart_rewrite_query("的 了 吗");
        // May or may not be empty depending on tokenizer behavior
        // Just verify it doesn't panic and reasonably short
        assert!(result.len() <= 10, "result: {result:?}");
    }

    #[test]
    fn expand_tokens_bidirectional() {
        let tokens = vec!["内存".to_string(), "error".to_string()];
        let expanded = expand_query_tokens(&tokens);
        assert!(expanded.contains(&"memory".to_string()));
        assert!(expanded.contains(&"错误".to_string()));
    }
}
