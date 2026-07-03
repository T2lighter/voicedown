//! 统一 JSON 配置持久化 module（D 候选）。
//!
//! **纯持久化层**：`load<T>` / `save<T>` / `exists`，`base_dir` 注入（产线
//! `%USERPROFILE%\Documents\VoiceDown`、测试 temp 目录）。自动建目录、缺失/损坏降 `Default`。
//! LLM 配置 / 导出配置 / 词典三处共享，消除各自 path+load+save+建目录的孪生样板。
//!
//! 边界：只管文件 IO。词典的内存缓存（`USER_DICTIONARY` + 即时生效）不在此 module，
//! 属词典应用层；词典「首次缺失建空模板」是产品决策，由调用方用 `exists` 判定后自己 `save(default)`。

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::PathBuf;

/// JSON 配置仓库（持 `base_dir`，所有配置文件落于其下，文件名 = `{name}.json`）。
pub struct JsonConfigStore {
    base_dir: PathBuf,
}

impl JsonConfigStore {
    /// 测试注入用（产线走 [`Default::default`]，读 `USERPROFILE`）。仅 test build。
    #[cfg(test)]
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// `{name}.json` 完整路径（内部统一加 `.json` 后缀）。
    fn path_for(&self, name: &str) -> PathBuf {
        self.base_dir.join(format!("{name}.json"))
    }

    /// 加载 `{name}.json`：缺失静默降默认（首次使用常态）；损坏/读取失败 → log + `T::default()`。
    pub fn load<T: Default + DeserializeOwned>(&self, name: &str) -> T {
        let path = self.path_for(name);
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<T>(&content) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[config:{name}] JSON 解析失败: {e}，使用默认");
                    T::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
            Err(e) => {
                eprintln!("[config:{name}] 文件读取失败: {e}，使用默认");
                T::default()
            }
        }
    }

    /// 写回 `{name}.json`（自动建父目录，pretty JSON）。失败返回错误字符串。
    pub fn save<T: Serialize>(&self, name: &str, value: &T) -> Result<(), String> {
        let path = self.path_for(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
        std::fs::write(&path, body).map_err(|e| e.to_string())
    }

    /// `{name}.json` 是否存在（词典「建空模板」判定用）。
    pub fn exists(&self, name: &str) -> bool {
        self.path_for(name).exists()
    }
}

impl Default for JsonConfigStore {
    /// 产线：`%USERPROFILE%\Documents\VoiceDown`。
    fn default() -> Self {
        let user_profile = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());
        Self {
            base_dir: PathBuf::from(format!("{}\\Documents\\VoiceDown", user_profile)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    /// 测试用配置类型（需 Default + Serialize + DeserializeOwned）。
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestCfg {
        name: String,
        count: u32,
    }
    impl Default for TestCfg {
        fn default() -> Self {
            Self { name: "default".into(), count: 0 }
        }
    }

    /// 唯一 temp 目录（带 PID 防并发冲突，cargo test 默认多线程）。
    fn unique_temp(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("voicedown_jcs_{}_{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn roundtrip_save_then_load() {
        let dir = unique_temp("roundtrip");
        let store = JsonConfigStore::new(dir.clone());
        let cfg = TestCfg { name: "abc".into(), count: 42 };
        store.save("llm_config", &cfg).unwrap();
        let back: TestCfg = store.load("llm_config");
        assert_eq!(back, cfg, "save 后 load 应往返一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = unique_temp("missing");
        let store = JsonConfigStore::new(dir.clone());
        let cfg: TestCfg = store.load("never_saved");
        assert_eq!(cfg, TestCfg::default(), "文件缺失应降默认");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_corrupt_returns_default() {
        let dir = unique_temp("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.json"), "not json {{{").unwrap();
        let store = JsonConfigStore::new(dir.clone());
        let cfg: TestCfg = store.load("bad");
        assert_eq!(cfg, TestCfg::default(), "损坏 JSON 应降默认（不崩）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let root = std::env::temp_dir().join(format!("voicedown_jcs_{}_mkdir", std::process::id()));
        let nested = root.join("deep");
        let _ = std::fs::remove_dir_all(&root);
        let store = JsonConfigStore::new(nested.clone());
        store.save("llm_config", &TestCfg::default()).unwrap();
        assert!(nested.join("llm_config.json").exists(), "应自动建父目录 + .json 后缀");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn exists_false_before_save_true_after() {
        let dir = unique_temp("exists");
        let store = JsonConfigStore::new(dir.clone());
        assert!(!store.exists("llm_config"), "save 前不存在");
        store.save("llm_config", &TestCfg::default()).unwrap();
        assert!(store.exists("llm_config"), "save 后存在");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dictionary_empty_map_roundtrip() {
        // 词典语义：BTreeMap<String,String> 默认 = 空 map；建空模板后 load 仍空。
        let dir = unique_temp("dict");
        let store = JsonConfigStore::new(dir.clone());
        let empty: BTreeMap<String, String> = BTreeMap::new();
        store.save("dictionary", &empty).unwrap();
        let back: BTreeMap<String, String> = store.load("dictionary");
        assert!(back.is_empty(), "空词典往返应仍空");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
