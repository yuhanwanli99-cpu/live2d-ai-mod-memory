//! 会话注入槽生命周期的回归（0.4.4）：未命中**保留**上一轮块、命中**整段替换**、
//! 注入开关关闭清槽；以及用户原文按换行**一条一行**落盘、检索仍用整段原文。
//!
//! 为什么单列一个文件：`tests.rs` 已 766 行（测试文件口径 ≤800），这三组主题又都
//! 围绕「注入槽 / 记录形状」，放在一起便于逐条对照。

use super::*;
use crate::test_support::*;

/// 该会话桶里的有效记录（按落盘顺序）。
fn session_records(dir: &std::path::Path, session: &str) -> Vec<MemoryRecord> {
    let store = JsonlStore::new(
        crate::resolve_session_store_path(&dir.join("memory.jsonl"), session).expect("合法会话 id"),
    );
    store.load(0).expect("桶可读").records
}

/// 未命中且无摘要：**不清槽**，上一轮注入块原样保留。
#[test]
fn miss_keeps_the_previous_injection_block() {
    let dir = temp_dir("keep-slot");
    let host = FakeHost::new("基础人设");
    let mut rt = runtime(&dir, &host, serde_json::json!({"top_k": 3}));
    rt.on_scoped_event(ModEventTopic::TurnPrompt, "我喜欢薄荷", Some("s1"))
        .unwrap();
    rt.on_scoped_event(ModEventTopic::TurnPrompt, "薄荷怎么样", Some("s1"))
        .unwrap();
    let before = host
        .sessions()
        .slot(SESSION_PROMPT_OWNER_MEMORY, "s1")
        .expect("第二句命中上一句，应注入");
    assert!(before.contains("- [用户] 我喜欢薄荷"), "{before}");
    let injects = rt.state_json().unwrap()["injects"].clone();

    // 第三句与桶里内容词面不重叠 → 本轮未命中且无摘要。
    rt.on_scoped_event(ModEventTopic::TurnPrompt, "量子力学导论", Some("s1"))
        .unwrap();
    assert_eq!(
        host.sessions()
            .slot(SESSION_PROMPT_OWNER_MEMORY, "s1")
            .as_deref(),
        Some(before.as_str()),
        "未命中不得清掉上一轮注入块"
    );
    let snap = rt.state_json().unwrap();
    assert_eq!(snap["last_hits"], 0);
    assert_eq!(snap["injects"], injects, "保留 ≠ 重新注入");
    assert!(
        host.logs()
            .iter()
            .any(|l| l.contains("本轮未命中，保留上一轮注入块")),
        "必须留下可排障的说明: {:?}",
        host.logs()
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// 新命中：`set_owned` **整段替换**旧块（不残留上一轮的行）。
#[test]
fn hit_replaces_the_whole_block_instead_of_merging() {
    let dir = temp_dir("replace-slot");
    let host = FakeHost::new("基础人设");
    let mut rt = runtime(&dir, &host, serde_json::json!({"top_k": 3}));
    for text in ["我喜欢薄荷", "薄荷怎么样", "量子力学导论"] {
        rt.on_scoped_event(ModEventTopic::TurnPrompt, text, Some("s1"))
            .unwrap();
    }
    let old = host
        .sessions()
        .slot(SESSION_PROMPT_OWNER_MEMORY, "s1")
        .expect("第二句应注入");
    assert!(old.contains("- [用户] 我喜欢薄荷"), "{old}");

    // 第四句只命中「量子力学导论」——新块必须整段替换，不留薄荷那两条。
    rt.on_scoped_event(ModEventTopic::TurnPrompt, "量子力学", Some("s1"))
        .unwrap();
    let new = host
        .sessions()
        .slot(SESSION_PROMPT_OWNER_MEMORY, "s1")
        .expect("第四句应注入");
    assert!(new.contains("- [用户] 量子力学导论"), "{new}");
    assert!(!new.contains("我喜欢薄荷"), "旧块必须被整段替换: {new}");
    assert_ne!(old, new);
    assert_eq!(rt.state_json().unwrap()["injects"], 2);
    let _ = std::fs::remove_dir_all(dir);
}

/// 注入开关关掉：**仍然清**该会话的槽（残留不能继续喂模型）。
#[test]
fn disabled_injection_still_clears_the_session_slot() {
    let dir = temp_dir("disabled-clear");
    let host = FakeHost::new("基础人设");
    let mut rt = runtime(&dir, &host, serde_json::json!({"top_k": 3}));
    rt.on_scoped_event(ModEventTopic::TurnPrompt, "我喜欢薄荷", Some("s1"))
        .unwrap();
    rt.on_scoped_event(ModEventTopic::TurnPrompt, "薄荷怎么样", Some("s1"))
        .unwrap();
    assert!(
        host.sessions()
            .slot(SESSION_PROMPT_OWNER_MEMORY, "s1")
            .is_some(),
        "先注入一块"
    );
    // 运行期配置不可热改；直接改字段模拟「开关关掉后的下一轮」（真实装配里
    // 是重建 runtime，会话槽留在宿主的 SessionScopeStore 里）。
    rt.config.enabled_injection = false;
    rt.on_scoped_event(ModEventTopic::TurnPrompt, "再聊聊薄荷", Some("s1"))
        .unwrap();
    assert!(
        host.sessions()
            .slot(SESSION_PROMPT_OWNER_MEMORY, "s1")
            .is_none(),
        "注入开关关闭必须清掉该会话槽"
    );
    assert_eq!(rt.state_json().unwrap()["injects"], 1, "关闭后不再注入");
    assert!(host.patches().is_empty(), "清槽不得写全局 persona");
    assert!(
        host.logs().iter().any(|l| l.contains("注入开关已关闭")),
        "{:?}",
        host.logs()
    );
    assert_eq!(rt.state_json().unwrap()["writes"], 3, "记忆照记");
    let _ = std::fs::remove_dir_all(dir);
}

/// 用户原文按换行拆成多条非空记录（一条一行），同轮共用一个 turn。
#[test]
fn multiline_prompt_is_remembered_one_record_per_nonempty_line() {
    let dir = temp_dir("multiline");
    let host = FakeHost::new("基础人设");
    let mut rt = runtime(&dir, &host, serde_json::json!({}));
    rt.on_scoped_event(
        ModEventTopic::TurnPrompt,
        "第一行薄荷\n\n   \n第二行\r\n第三行量子力学",
        Some("s1"),
    )
    .unwrap();

    let records = session_records(&dir, "s1");
    let texts: Vec<&str> = records.iter().map(|record| record.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["第一行薄荷", "第二行", "第三行量子力学"],
        "一条一行、空行 / 纯空白行跳过"
    );
    assert!(
        records
            .iter()
            .all(|record| record.role == crate::strategy::MemoryRole::User),
        "用户侧记录: {records:?}"
    );
    assert!(
        records.iter().all(|record| record.turn == 1),
        "同轮共用一个 turn: {records:?}"
    );
    let snap = rt.state_json().unwrap();
    assert_eq!(snap["turns_seen"], 1, "一次 TurnPrompt = 一轮");
    assert_eq!(snap["writes"], 3, "三条非空行 = 三条记录");
    let _ = std::fs::remove_dir_all(dir);
}

/// 检索仍用**整段**原文（不是最后一行），且本轮刚写入的行不自命中。
#[test]
fn multiline_prompt_retrieves_with_the_whole_text() {
    let dir = temp_dir("multiline-retrieve");
    let host = FakeHost::new("基础人设");
    let mut rt = runtime(&dir, &host, serde_json::json!({"top_k": 3}));
    rt.on_scoped_event(ModEventTopic::TurnPrompt, "我喜欢薄荷茶", Some("s1"))
        .unwrap();
    rt.on_scoped_event(
        ModEventTopic::TurnPrompt,
        "第一行薄荷\n第二行\n第三行量子力学",
        Some("s1"),
    )
    .unwrap();
    let block = host
        .sessions()
        .slot(SESSION_PROMPT_OWNER_MEMORY, "s1")
        .expect("整段原文里含薄荷，应命中历史记录");
    assert!(block.contains("- [用户] 我喜欢薄荷茶"), "{block}");
    assert!(!block.contains("第一行薄荷"), "当轮正文不得自命中: {block}");
    assert!(!block.contains("第三行量子力学"), "{block}");
    assert_eq!(rt.state_json().unwrap()["writes"], 4);
    let _ = std::fs::remove_dir_all(dir);
}
