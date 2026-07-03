//! VoiceDown - 用户词典应用层
//!
//! 词典内存缓存（`USER_DICTIONARY`，全局可重载，`set_dictionary` 后即时生效）+ 懒加载
//!（首次访问从 `dictionary.json` 读，缺失建空模板）+ IPC 辅助（快照 / 存储）。持久化经
//! `JsonConfigStore`（D 候选），纯替换逻辑在 `text_postprocess::apply_dictionary`
//!（最长匹配 + ASCII 词边界，G 候选）。A4 候选自 lib.rs 迁出（G 候选 Q1 决策：词典应用层留 A）。
//!
//! 词典文件路径：`%USERPROFILE%\Documents\VoiceDown\dictionary.json`，格式 `{"key":"value"}`。

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

use crate::json_config_store::JsonConfigStore;
// 别名避让：本模块暴露同名壳 `apply_dictionary`（取缓存 + 调纯函数），纯函数用 `apply_dict`。
use crate::text_postprocess::apply_dictionary as apply_dict;

/// 用户词典（全局可重载，ISSUE-4：set_dictionary 后即时生效）。
static USER_DICTIONARY: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());
static DICT_LOADED: OnceLock<()> = OnceLock::new();

/// 读取 dictionary.json：缺失自动建空模板（首次使用正常态），解析/IO 错降级空词典。
///
/// 持久化经 JsonConfigStore（D 候选）；内存缓存由 ensure_dict_loaded 管。
fn read_dict_file() -> BTreeMap<String, String> {
    let store = JsonConfigStore::default();
    if !store.exists("dictionary") {
        // 文件不存在是首次使用的正常状态（非错误）：自动创建空词典模板（{}），
        // 既消除每次启动的告警，也方便用户直接编辑该文件添加自定义词。
        eprintln!("[VoiceDown] 词典文件不存在（首次使用），自动创建空模板");
        if let Err(e) = store.save("dictionary", &BTreeMap::<String, String>::new()) {
            eprintln!("[VoiceDown] 创建空词典模板失败: {}（使用空词典继续）", e);
        }
    }
    let dict: BTreeMap<String, String> = store.load("dictionary");
    if !dict.is_empty() {
        eprintln!("[VoiceDown] 用户词典已加载: {} 条规则", dict.len());
    }
    dict
}

/// 首次访问从文件加载词典到全局 RwLock（仅一次）。
/// set_dictionary 直接更新内存（不重读文件），保证前端改词后下次 ASR 即时生效。
fn ensure_dict_loaded() {
    DICT_LOADED.get_or_init(|| {
        *USER_DICTIONARY.write().unwrap() = read_dict_file();
    });
}

/// 对文本应用词典替换（t2s 之前调用，保持现有调用顺序）。
///
/// 壳：懒加载缓存 + 读全局词典 + 委托纯函数 `text_postprocess::apply_dictionary`
///（最长匹配 + ASCII 词边界，ISSUE-4 / REM-08）。
pub(crate) fn apply_dictionary(text: &str) -> String {
    ensure_dict_loaded();
    let dict = USER_DICTIONARY.read().unwrap();
    apply_dict(text, &dict)
}

/// 取词典快照（IPC get_dictionary 用）。
pub(crate) fn get_dictionary_snapshot() -> BTreeMap<String, String> {
    ensure_dict_loaded();
    USER_DICTIONARY.read().unwrap().clone()
}

/// 持久化词典到文件 + 更新内存（即时生效）。IPC set_dictionary 用。
pub(crate) fn store_dictionary(dict: BTreeMap<String, String>) -> Result<(), String> {
    JsonConfigStore::default().save("dictionary", &dict)?;
    *USER_DICTIONARY.write().unwrap() = dict;
    Ok(())
}
