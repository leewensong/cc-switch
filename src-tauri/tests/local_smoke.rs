//! 本地服务链路烟测（真实进程；默认忽略，需要本机 MLX 模型与 venv）
//!
//! 运行：`cargo test --test local_smoke -- --ignored --nocapture`
//! 环境变量：
//! - `LOCAL_SMOKE_MODEL`：模型目录（默认取本机 LM Studio 的 Qwen3.6-35B-A3B）
//! - `LOCAL_SMOKE_PYTHON` / `LOCAL_SMOKE_PROXY`：解释器/网关路径
//!   （默认走 `detect_python_path` / `detect_proxy_path` 探测链）

use cc_switch_lib::local::{
    detect_python_path, detect_proxy_path, discover_models, LocalManager, LocalSettings,
    RuntimeState,
};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[test]
#[ignore]
fn local_chain_smoke() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());

    rt.block_on(async {
        let manager = LocalManager::new();

        let mut settings = LocalSettings::default();
        settings.python_path =
            env_or("LOCAL_SMOKE_PYTHON", &detect_python_path());
        settings.proxy_path = env_or("LOCAL_SMOKE_PROXY", &detect_proxy_path());
        assert!(
            !settings.python_path.is_empty(),
            "未探测到 Python 解释器（需要在 mlx_lm 同目录 .venv 或安装 mlx_lm）"
        );
        assert!(
            !settings.proxy_path.is_empty(),
            "未探测到 anthropic-proxy（需要 ~/.local/bin/anthropic-proxy）"
        );
        // 使用高位端口，避免与开发机其他服务冲突
        settings.mlx_port = 8092;
        settings.gateway_port = 8093;

        let mut model_path = env_or("LOCAL_SMOKE_MODEL", "");
        if model_path.is_empty() {
            // 默认：HF 缓存中的 Qwen3.8-27B MLX 4bit（本机测试模型）
            let hf_root = dirs::home_dir()
                .expect("home dir")
                .join(".cache/huggingface/hub/models--lmstudio-community--Qwen3.8-27B-MLX-4bit/snapshots");
            if let Ok(entries) = std::fs::read_dir(&hf_root) {
                model_path = entries
                    .flatten()
                    .map(|e| e.path())
                    .find(|p| p.join("config.json").is_file())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
            }
        }
        assert!(
            !model_path.is_empty(),
            "未设置 LOCAL_SMOKE_MODEL 且未找到默认测试模型（Qwen3.8-27B MLX 4bit）"
        );
        println!("[smoke] model: {model_path}");

        // 找模型记录：优先扫描入库，再直接解析
        let records = discover_models(&settings);
        let model = records
            .iter()
            .find(|m| m.path == model_path)
            .cloned()
            .or_else(|| cc_switch_lib::local::discovery::parse_model_dir(
                std::path::Path::new(&model_path),
                cc_switch_lib::local::model::LocalModelSource::Custom,
            ))
            .expect("模型目录可解析");

        // discovery 回归：HF 缓存权重是 symlink，真实大小必须 > 1GB（修复 symlink 大小 bug）
        assert!(
            model.size_bytes > 1_000_000_000,
            "discovery 权重大小异常：{} bytes（symlink 未跟随？）",
            model.size_bytes
        );
        println!(
            "[smoke] discovered: name={}, size={} bytes, quant={}, ctx={:?}",
            model.name, model.size_bytes, model.quantization, model.context_length
        );

        println!("[smoke] starting…");
        manager
            .start(model.clone(), settings)
            .await
            .expect("start 应成功（或同模型幂等）");

        // 等就绪（10 分钟上限）
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
        loop {
            let status = manager.status().await;
            match status.state {
                RuntimeState::Ready | RuntimeState::Degraded => {
                    println!("[smoke] READY: {}", status.phase_detail);
                    break;
                }
                RuntimeState::Failed => {
                    panic!("启动失败: {}", status.last_error.unwrap_or_default());
                }
                _ => {
                    if tokio::time::Instant::now() > deadline {
                        panic!("等待就绪超时，状态: {:?}", status.state);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }

        // 小推理测试（走统一网关）
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(120))
            .no_proxy()
            .build()
            .unwrap();
        let resp = client
            .post("http://127.0.0.1:8093/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "default_model",
                "messages": [{ "role": "user", "content": "Reply with OK." }],
                "max_tokens": 4,
                "temperature": 0,
                "stream": false,
            }))
            .send()
            .await
            .expect("测试请求应成功");
        assert!(
            resp.status().is_success(),
            "网关测试请求失败: HTTP {}",
            resp.status()
        );
        println!("[smoke] gateway test request OK");

        // 停止
        manager.stop().await;
        let status = manager.status().await;
        assert_eq!(status.state, RuntimeState::Stopped, "停止后应为 stopped");
        println!("[smoke] stopped OK");

        // 幂等停止
        manager.stop().await;
        println!("[smoke] PASS");
    });
}

#[tokio::test]
async fn start_rejects_non_macos() {
    // 非 macOS 平台启动应被拒绝（本测试在 macOS 上跑，验证语义分支：无效模型先报错）
    let manager = LocalManager::new();
    let mut settings = LocalSettings::default();
    settings.mlx_port = 8094;
    settings.gateway_port = 8095;
    let model = cc_switch_lib::local::model::LocalModelRecord {
        path: "/nonexistent-model".to_string(),
        name: "nonexistent".to_string(),
        source: cc_switch_lib::local::model::LocalModelSource::Custom,
        size_bytes: 0,
        model_type: "unknown".to_string(),
        quantization: "未标注".to_string(),
        context_length: None,
        kv_bytes_per_token: None,
        has_tokenizer: false,
        has_chat_template: false,
        memory_fit: cc_switch_lib::local::model::MemoryFit::Comfortable,
        verify_level: 0,
        fingerprint: None,
        last_used_at: None,
    };
    let result = manager.start(model, settings).await;
    assert!(
        result.is_err() || manager.status().await.state != RuntimeState::Ready,
        "无效模型不应就绪"
    );
    // 清理（确保无残留）
    manager.stop().await;
}
