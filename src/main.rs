use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State},
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::Instant,
};
use tokio::sync::{Mutex, broadcast};
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::{error, info, warn};
use uuid::Uuid;
use futures_util::{StreamExt, SinkExt};

// Constants for persistence
const PROOFS_DB_FILE: &str = "./proofs_db.json";
const VERIFICATIONS_DB_FILE: &str = "./verifications_db.json";

#[derive(Clone)]
struct AppState {
    zkengine_binary: String,
    wasm_dir: String,
    proofs_dir: String,
    proof_store: Arc<Mutex<HashMap<String, ProofRecord>>>,
    verification_store: Arc<Mutex<Vec<VerificationRecord>>>,
    tx: broadcast::Sender<WsMessage>,
    langchain_url: String,
    session_store: Arc<Mutex<HashMap<String, String>>>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ProofRecord {
    id: String,
    timestamp: DateTime<Utc>,
    metadata: ProofMetadata,
    metrics: ProofMetrics,
    status: ProofStatus,
    file_path: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct VerificationRecord {
    id: String,
    proof_id: String,
    timestamp: DateTime<Utc>,
    is_valid: bool,
    verification_time_secs: f64,
    error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ProofMetadata {
    wasm_path: String,
    function: String,
    arguments: Vec<String>,
    step_size: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct ProofMetrics {
    generation_time_secs: f64,
    file_size_mb: f64,
    file_hash: String,
    peak_memory_mb: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "lowercase")]
enum ProofStatus {
    Pending,
    Running,
    Complete,
    Failed(String),
}

#[derive(Serialize, Clone)]
struct WsMessage {
    #[serde(rename = "type")]
    msg_type: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ChatMessage {
    message: String,
}

// LangChain service integration
#[derive(Debug, Serialize, Deserialize)]
struct LangChainRequest {
    message: String,
    session_id: Option<String>,
    context: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LangChainResponse {
    intent: Option<LangChainIntent>,
    response: String,
    session_id: String,
    requires_proof: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct LangChainIntent {
    function: String,
    arguments: Vec<String>,
    step_size: u64,
    explanation: String,
    complexity_reasoning: Option<String>,
}

// Convert city names to numeric codes for zkEngine
fn convert_location_args(args: &[String]) -> Vec<String> {
    args.iter().enumerate().map(|(i, arg)| {
        if i == 0 {  // First argument is city name
            match arg.to_lowercase().as_str() {
                "san francisco" | "sf" => "1".to_string(),
                "new york" | "nyc" => "2".to_string(),
                "london" => "3".to_string(),
                _ => arg.clone()
            }
        } else {
            arg.clone()  // Keep device IDs and other args as-is
        }
    }).collect()
}

// Persistence functions
async fn save_proofs_to_disk(proofs: &HashMap<String, ProofRecord>) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(proofs)?;
    tokio::fs::write(PROOFS_DB_FILE, json).await?;
    Ok(())
}

async fn load_proofs_from_disk() -> Result<HashMap<String, ProofRecord>, Box<dyn std::error::Error>> {
    if Path::new(PROOFS_DB_FILE).exists() {
        let json = tokio::fs::read_to_string(PROOFS_DB_FILE).await?;
        let proofs: HashMap<String, ProofRecord> = serde_json::from_str(&json)?;
        Ok(proofs)
    } else {
        Ok(HashMap::new())
    }
}

async fn save_verifications_to_disk(verifications: &Vec<VerificationRecord>) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(verifications)?;
    tokio::fs::write(VERIFICATIONS_DB_FILE, json).await?;
    Ok(())
}

async fn load_verifications_from_disk() -> Result<Vec<VerificationRecord>, Box<dyn std::error::Error>> {
    if Path::new(VERIFICATIONS_DB_FILE).exists() {
        let json = tokio::fs::read_to_string(VERIFICATIONS_DB_FILE).await?;
        let verifications: Vec<VerificationRecord> = serde_json::from_str(&json)?;
        Ok(verifications)
    } else {
        Ok(Vec::new())
    }
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let zkengine_binary = std::env::var("ZKENGINE_BINARY")
        .unwrap_or_else(|_| "/home/hshadab/zkengine/zkEngine_dev/wasm_file".to_string());
    let wasm_dir = std::env::var("WASM_DIR")
        .unwrap_or_else(|_| "/home/hshadab/agentkit/zkengine/example_wasms".to_string());
    let proofs_dir = std::env::var("PROOFS_DIR")
        .unwrap_or_else(|_| "./proofs".to_string());
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8001".to_string())
        .parse::<u16>()
        .unwrap_or(8001);
    let langchain_url = std::env::var("LANGCHAIN_SERVICE_URL")
        .unwrap_or_else(|_| "http://localhost:8002".to_string());

    // Create directories
    fs::create_dir_all(&proofs_dir).ok();

    // Create broadcast channel for WebSocket messages
    let (tx, _rx) = broadcast::channel::<WsMessage>(1000);

    // Load existing proofs and verifications
    let stored_proofs = load_proofs_from_disk().await.unwrap_or_else(|e| {
        warn!("Failed to load proofs from disk: {}", e);
        HashMap::new()
    });
    
    let stored_verifications = load_verifications_from_disk().await.unwrap_or_else(|e| {
        warn!("Failed to load verifications from disk: {}", e);
        Vec::new()
    });

    info!("Loaded {} proofs and {} verifications from disk", 
          stored_proofs.len(), stored_verifications.len());

    let state = AppState {
        zkengine_binary,
        wasm_dir,
        proofs_dir,
        proof_store: Arc::new(Mutex::new(stored_proofs)),
        verification_store: Arc::new(Mutex::new(stored_verifications)),
        tx: tx.clone(),
        langchain_url,
        session_store: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/ws", get(websocket_handler))
        .route("/api/health", get(health_check))
        .route("/api/langchain/health", get(langchain_health))
        .route("/api/proofs", get(list_proofs))
        .route("/api/proofs/:id", get(get_proof))
        .route("/api/proofs/generate", post(generate_proof))
        .route("/api/cleanup", post(cleanup_old_proofs))
        .nest_service("/static", ServeDir::new("static"))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    info!("🚀 zkEngine Agent Kit running on http://{}", addr);
    
    axum::Server::bind(&addr.parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn serve_index() -> impl IntoResponse {
    Html(include_str!("../static/index.html"))
}

async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let binary_exists = Path::new(&state.zkengine_binary).exists();
    let wasm_dir_exists = Path::new(&state.wasm_dir).exists();
    
    Json(json!({
        "status": "ok",
        "zkengine_binary": state.zkengine_binary,
        "binary_exists": binary_exists,
        "wasm_dir": state.wasm_dir,
        "wasm_dir_exists": wasm_dir_exists,
        "proofs_dir": state.proofs_dir,
        "langchain_url": state.langchain_url,
    }))
}

async fn langchain_health(State(state): State<AppState>) -> impl IntoResponse {
    let client = reqwest::Client::new();
    match client.get(&format!("{}/health", state.langchain_url)).send().await {
        Ok(response) => {
            if response.status().is_success() {
                let health_data: serde_json::Value = response.json().await.unwrap_or_default();
                Json(json!({
                    "langchain_service": "healthy",
                    "details": health_data
                }))
            } else {
                Json(json!({
                    "langchain_service": "unhealthy",
                    "error": "Service returned non-200 status"
                }))
            }
        },
        Err(e) => Json(json!({
            "langchain_service": "unreachable",
            "error": e.to_string()
        }))
    }
}

async fn list_proofs(State(state): State<AppState>) -> impl IntoResponse {
    let proofs = state.proof_store.lock().await;
    let proofs_list: Vec<&ProofRecord> = proofs.values().collect();
    Json(json!({
        "proofs": proofs_list,
        "count": proofs_list.len()
    }))
}

async fn get_proof(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let proofs = state.proof_store.lock().await;
    match proofs.get(&id) {
        Some(proof) => Json(json!({
            "success": true,
            "proof": proof
        })),
        None => Json(json!({
            "success": false,
            "error": "Proof not found"
        }))
    }
}

async fn generate_proof(
    State(state): State<AppState>,
    Json(request): Json<serde_json::Value>,
) -> impl IntoResponse {
    let proof_id = Uuid::new_v4().to_string();
    
    // Parse request
    let wasm_file = request["wasm_file"].as_str().unwrap_or("fibonacci.wat");
    let function = request["function"].as_str().unwrap_or("main");
    let args = request["arguments"].as_array()
        .map(|arr| arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect::<Vec<_>>())
        .unwrap_or_default();
    let step_size = request["step_size"].as_u64().unwrap_or(50);
    
    let metadata = ProofMetadata {
        wasm_path: format!("{}/{}", state.wasm_dir, wasm_file),
        function: function.to_string(),
        arguments: args.clone(),
        step_size,
    };
    
    // Create proof record
    let proof_record = ProofRecord {
        id: proof_id.clone(),
        timestamp: Utc::now(),
        metadata: metadata.clone(),
        metrics: ProofMetrics {
            generation_time_secs: 0.0,
            file_size_mb: 0.0,
            file_hash: String::new(),
            peak_memory_mb: None,
        },
        status: ProofStatus::Pending,
        file_path: None,
    };
    
    state.proof_store.lock().await.insert(proof_id.clone(), proof_record.clone());
    
    // Save to disk
    {
        let proofs = state.proof_store.lock().await;
        if let Err(e) = save_proofs_to_disk(&*proofs).await {
            error!("Failed to save proofs to disk: {}", e);
        }
    }
    
    // Spawn proof generation
    let state_clone = state.clone();
    let proof_id_clone = proof_id.clone();
    tokio::spawn(async move {
        generate_real_proof(state_clone, proof_id_clone, metadata, args).await;
    });
    
    Json(json!({
        "success": true,
        "proof_id": proof_id,
        "message": "Proof generation started"
    }))
}

async fn cleanup_old_proofs(State(state): State<AppState>) -> impl IntoResponse {
    let mut proofs = state.proof_store.lock().await;
    let cutoff = Utc::now() - chrono::Duration::days(7); // Keep last 7 days
    
    let before_count = proofs.len();
    proofs.retain(|_, proof| proof.timestamp > cutoff);
    let after_count = proofs.len();
    
    if let Err(e) = save_proofs_to_disk(&*proofs).await {
        error!("Failed to save proofs after cleanup: {}", e);
    }
    
    Json(json!({
        "message": "Cleaned up old proofs",
        "removed": before_count - after_count,
        "remaining": after_count
    }))
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| websocket_connection(socket, state))
}

async fn websocket_connection(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    
    // Subscribe to broadcast channel
    let mut rx = state.tx.subscribe();
    
    // Send welcome message
    let welcome = WsMessage {
        msg_type: "message".to_string(),
        content: "Connected to zkEngine Agent Kit! Try 'prove device location in San Francisco' or 'help'.".to_string(),
        data: None,
    };
    sender.send(Message::Text(serde_json::to_string(&welcome).unwrap())).await.ok();
    
    // Spawn task to receive broadcast messages
    let send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(serde_json::to_string(&msg).unwrap())).await.is_err() {
                break;
            }
        }
    });
    
    while let Some(msg) = receiver.next().await {
        if let Ok(msg) = msg {
            match msg {
                Message::Text(text) => {
                    if let Ok(chat_msg) = serde_json::from_str::<ChatMessage>(&text) {
                        let response = process_nl_command(&state, &chat_msg.message).await;
                        // Only send a message if there's content
                        if !response.message.is_empty() {
                            let ws_msg = WsMessage {
                                msg_type: "message".to_string(),
                                content: response.message,
                                data: response.data,
                            };
                            // Broadcast to all clients
                            let _ = state.tx.send(ws_msg);
                        } else if let Some(data) = response.data {
                            // Send data-only message if no text content
                            let ws_msg = WsMessage {
                                msg_type: "message".to_string(),
                                content: String::new(),
                                data: Some(data),
                            };
                            let _ = state.tx.send(ws_msg);
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    }
    
    send_task.abort();
}

struct NlResponse {
    message: String,
    data: Option<serde_json::Value>,
}

// Add LangChain processing function
async fn process_with_langchain(
    langchain_url: &str, 
    message: &str, 
    session_id: Option<String>
) -> Result<LangChainResponse, anyhow::Error> {
    let client = reqwest::Client::new();
    
    let request = LangChainRequest {
        message: message.to_string(),
        session_id,
        context: None,
    };
    
    let response = client
        .post(&format!("{}/chat", langchain_url))
        .json(&request)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?;
    
    if !response.status().is_success() {
        let error_text = response.text().await?;
        return Err(anyhow::anyhow!("LangChain service error: {}", error_text));
    }
    
    let langchain_response: LangChainResponse = response.json().await?;
    Ok(langchain_response)
}

// UPDATED: process_nl_command function with custom proof support
async fn process_nl_command(state: &AppState, input: &str) -> NlResponse {
    let input_lower = input.to_lowercase();
    
    // PRIORITY: Handle list and verify commands BEFORE LangChain
    if input_lower.contains("list") && (input_lower.contains("proof") || input_lower.contains("all")) {
        info!("Handling list proofs command");
        let proofs = state.proof_store.lock().await;
        let proofs_list: Vec<&ProofRecord> = proofs.values().collect();
        info!("Found {} proofs", proofs_list.len());
        
        return NlResponse {
            message: format!("Found {} proofs in history", proofs_list.len()),
            data: Some(json!({
                "type": "proof_list",
                "proofs": proofs_list
            })),
        };
    }
    
    if input_lower.contains("list") && input_lower.contains("verification") {
        info!("Handling list verifications command");
        let verifications = state.verification_store.lock().await;
        
        return NlResponse {
            message: format!("Found {} verifications in history", verifications.len()),
            data: Some(json!({
                "type": "verification_list",
                "verifications": *verifications
            })),
        };
    }
    
    // Handle verification commands
    if input_lower.contains("verify") {
        // Extract proof ID if specified
        let proof_id = if input_lower.contains("proof") {
            // Pattern: "verify proof <id>"
            let parts: Vec<&str> = input.split_whitespace().collect();
            if parts.len() >= 3 {
                Some(parts[2].to_string())
            } else {
                None
            }
        } else {
            // Just "verify" - get the last proof
            let proofs = state.proof_store.lock().await;
            proofs.values()
                .filter(|p| matches!(p.status, ProofStatus::Complete))
                .max_by_key(|p| &p.timestamp)
                .map(|p| p.id.clone())
        };
        
        if let Some(id) = proof_id {
            info!("Starting verification for proof: {}", id);
            
            // Spawn verification task
            let state_clone = state.clone();
            let id_clone = id.clone();
            tokio::spawn(async move {
                verify_proof_async(state_clone, id_clone).await;
            });
            
            return NlResponse {
                message: format!("Starting verification for proof {}", &id[..8]),
                data: Some(json!({
                    "type": "verification_start",
                    "proof_id": id
                })),
            };
        } else {
            return NlResponse {
                message: "No proof found to verify. Generate a proof first or specify a proof ID.".to_string(),
                data: None,
            };
        }
    }
    
    // Handle custom proof commands
if input_lower.contains("prove custom") {
    // Extract WASM file
    // Pattern: "prove custom <wasm_file>" (no arguments needed now)
    let parts: Vec<&str> = input.split_whitespace().collect();
    
    // Find the wasm file (should be after "custom")
    let wasm_file = if let Some(custom_idx) = parts.iter().position(|&s| s == "custom") {
        if custom_idx + 1 < parts.len() {
            parts[custom_idx + 1].to_string()
        } else {
            "custom.wat".to_string()
        }
    } else {
        "custom.wat".to_string()
    };
    
    // No arguments needed - values are hardcoded in the C code
    let args: Vec<String> = vec!["0".to_string()];
    
    info!("Processing custom proof: wasm={}, args={:?} (dummy arg for hardcoded values)", wasm_file, args);
    
    let proof_id = Uuid::new_v4().to_string();
    let metadata = ProofMetadata {
        wasm_path: format!("{}/{}", state.wasm_dir, wasm_file),
        function: "main".to_string(),
        arguments: args.clone(),
        step_size: 50,
    };
    
    // Create proof record
    let proof_record = ProofRecord {
        id: proof_id.clone(),
        timestamp: Utc::now(),
        metadata: metadata.clone(),
        metrics: ProofMetrics {
            generation_time_secs: 0.0,
            file_size_mb: 0.0,
            file_hash: String::new(),
            peak_memory_mb: None,
        },
        status: ProofStatus::Pending,
        file_path: None,
    };
    
    state.proof_store.lock().await.insert(proof_id.clone(), proof_record);
    
    // Save to disk
    {
        let proofs = state.proof_store.lock().await;
        if let Err(e) = save_proofs_to_disk(&*proofs).await {
            error!("Failed to save proofs to disk: {}", e);
        }
    }
    
    // Send proof starting message
    let start_msg = WsMessage {
        msg_type: "message".to_string(),
        content: format!("Starting custom proof generation with WASM: {} (using hardcoded values)", wasm_file),
        data: Some(json!({ 
            "type": "proof_start",
            "proof_id": proof_id,
            "function": "main",
            "arguments": args,
            "wasm_file": wasm_file,
            "step_size": 50
        })),
    };
    let _ = state.tx.send(start_msg);
    
    // Spawn proof generation
    let state_clone = state.clone();
    let proof_id_clone = proof_id.clone();
    tokio::spawn(async move {
        generate_real_proof(state_clone, proof_id_clone, metadata, args).await;
    });
    
    return NlResponse {
        message: String::new(),
        data: None,
    };
}

    
    let session_id = Some("default".to_string());
    
    // First, ALWAYS try LangChain for ANY input to get natural language processing
    match process_with_langchain(&state.langchain_url, input, session_id.clone()).await {
        Ok(langchain_response) => {
            // ALWAYS send the natural language response first if it exists
            if !langchain_response.response.is_empty() {
                let nl_msg = WsMessage {
                    msg_type: "message".to_string(),
                    content: langchain_response.response.clone(),
                    data: Some(json!({ 
                        "session_id": langchain_response.session_id,
                        "from_langchain": true 
                    })),
                };
                // Send the natural language response immediately
                let _ = state.tx.send(nl_msg);
            }
            
            // Check for proof generation
            if langchain_response.requires_proof && langchain_response.intent.is_some() {
                let intent = langchain_response.intent.unwrap();
                
                // Map function name to WASM file
                let wasm_file = match intent.function.as_str() {
                    "prove_location" => "prove_location.wat",
                    "fibonacci" => "fib.wat",
                    "add" => "add.wat",
                    "multiply" => "multiply.wat",
                    "factorial" => "factorial_i32.wat",
                    "is_even" => "is_even.wat",
                    "square" => "square.wat",
                    "max" => "max.wat",
                    "count_until" => "count_until.wat",
                    "prove_kyc" => "prove_kyc.wat",
                    "prove_ai_content" => "prove_ai_content.wat",
                    _ => {
                        return NlResponse {
                            message: String::new(),
                            data: Some(json!({
                                "error": format!("Unknown function: {}", intent.function)
                            })),
                        };
                    }
                };
                
                let proof_id = Uuid::new_v4().to_string();
                let metadata = ProofMetadata {
                    wasm_path: format!("{}/{}", state.wasm_dir, wasm_file),
                    function: "main".to_string(),
                    arguments: intent.arguments.clone(),
                    step_size: intent.step_size,
                };
                
                // Create proof record
                let proof_record = ProofRecord {
                    id: proof_id.clone(),
                    timestamp: Utc::now(),
                    metadata: metadata.clone(),
                    metrics: ProofMetrics {
                        generation_time_secs: 0.0,
                        file_size_mb: 0.0,
                        file_hash: String::new(),
                        peak_memory_mb: None,
                    },
                    status: ProofStatus::Pending,
                    file_path: None,
                };
                
                state.proof_store.lock().await.insert(proof_id.clone(), proof_record);
                
                // Save to disk
                {
                    let proofs = state.proof_store.lock().await;
                    if let Err(e) = save_proofs_to_disk(&*proofs).await {
                        error!("Failed to save proofs to disk: {}", e);
                    }
                }
                
                // Convert arguments for location proofs
                let processed_args = if intent.function == "prove_location" {
                    convert_location_args(&intent.arguments)
                } else {
                    intent.arguments.clone()
                };
                
                // Send SINGLE proof starting message with correct format
                let start_msg = WsMessage {
                    msg_type: "message".to_string(),
                    content: format!("Starting proof generation for {} with arguments {:?}", intent.function, intent.arguments),
                    data: Some(json!({ 
                        "type": "proof_start",
                        "proof_id": proof_id,
                        "function": intent.function,
                        "arguments": intent.arguments,
                        "wasm_file": wasm_file,
                        "step_size": intent.step_size
                    })),
                };
                let _ = state.tx.send(start_msg);
                
                // Spawn proof generation
                let state_clone = state.clone();
                let proof_id_clone = proof_id.clone();
                tokio::spawn(async move {
                    generate_real_proof(state_clone, proof_id_clone, metadata, processed_args).await;
                });
                
                return NlResponse {
                    message: String::new(),
                    data: None,
                };
            }
            
            // Just conversation - response already sent
            return NlResponse {
                message: String::new(),
                data: None,
            };
        },
        Err(e) => {
            warn!("LangChain processing failed: {}", e);
            // Fall back to local command processing
        }
    }
    
    // Fallback for when LangChain is unavailable
    NlResponse {
        message: "LangChain service unavailable. Please check if it's running on port 8002.".to_string(),
        data: None,
    }
}

// FIXED: verify_proof_async function with correct command structure
async fn verify_proof_async(state: AppState, proof_id: String) {
    let start_time = Instant::now();
    
    // Get the proof record
    let proof_record = {
        let proofs = state.proof_store.lock().await;
        proofs.get(&proof_id).cloned()
    };
    
    let Some(proof) = proof_record else {
        let _ = state.tx.send(WsMessage {
            msg_type: "message".to_string(),
            content: format!("Proof {} not found", &proof_id[..8]),
            data: Some(json!({
                "type": "verification_complete",
                "proof_id": proof_id,
                "is_valid": false,
                "error": "Proof not found"
            })),
        });
        return;
    };
    
    // Check if proof is complete
    if !matches!(proof.status, ProofStatus::Complete) {
        let _ = state.tx.send(WsMessage {
            msg_type: "message".to_string(),
            content: format!("Proof {} is not complete yet", &proof_id[..8]),
            data: Some(json!({
                "type": "verification_complete", 
                "proof_id": proof_id,
                "is_valid": false,
                "error": "Proof not complete"
            })),
        });
        return;
    }
    
    // Get the proof file path
    let Some(proof_file_path) = &proof.file_path else {
        let _ = state.tx.send(WsMessage {
            msg_type: "message".to_string(),
            content: format!("Proof file not found for {}", &proof_id[..8]),
            data: Some(json!({
                "type": "verification_complete",
                "proof_id": proof_id, 
                "is_valid": false,
                "error": "Proof file not found"
            })),
        });
        return;
    };
    
    info!("Verifying proof {} using file {}", proof_id, proof_file_path);
    
    // Clone values for the blocking task
    let zkengine_binary = state.zkengine_binary.clone();
    let proof_file_path_clone = proof_file_path.clone();
    
    // Run verification in a blocking task
    let verification_result = tokio::task::spawn_blocking(move || {
        // Build correct verification command: wasm_file verify --step <STEP> <PROOF> <PUBLIC>
        let proof_dir = std::path::Path::new(&proof_file_path_clone).parent().unwrap();
        let public_file = proof_dir.join("public.json");
        
        let mut cmd = Command::new(&zkengine_binary);
        cmd.arg("verify")
            .arg("--step").arg("50")
            .arg(&proof_file_path_clone)  // proof.bin file
            .arg(&public_file);           // public.json file
        
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped());
        
        info!("Executing verification command: {:?}", cmd);
        cmd.output()
    }).await;
    
    let duration = start_time.elapsed();
    let verification_id = Uuid::new_v4().to_string();
    
    match verification_result {
        Ok(Ok(output)) => {
            let is_valid = output.status.success();
            let error_msg = if !is_valid {
                Some(String::from_utf8_lossy(&output.stderr).to_string())
            } else {
                None
            };
            
            // Create verification record
            let verification_record = VerificationRecord {
                id: verification_id.clone(),
                proof_id: proof_id.clone(),
                timestamp: Utc::now(),
                is_valid,
                verification_time_secs: duration.as_secs_f64(),
                error: error_msg.clone(),
            };
            
            // Store verification result
            {
                let mut verifications = state.verification_store.lock().await;
                verifications.push(verification_record);
                
                // Save to disk
                if let Err(e) = save_verifications_to_disk(&*verifications).await {
                    error!("Failed to save verifications to disk: {}", e);
                }
            }
            
            // Send verification result
            let result_message = if is_valid {
                format!("✅ Proof {} is VALID! Verified in {:.3}s", &proof_id[..8], duration.as_secs_f64())
            } else {
                format!("❌ Proof {} is INVALID. Error: {}", &proof_id[..8], error_msg.clone().unwrap_or_default())
            };
            
            let _ = state.tx.send(WsMessage {
                msg_type: "message".to_string(),
                content: result_message,
                data: Some(json!({
                    "type": "verification_complete",
                    "verification_id": verification_id,
                    "proof_id": proof_id,
                    "is_valid": is_valid,
                    "verification_time_secs": duration.as_secs_f64(),
                    "error": error_msg
                })),
            });
        }
        Ok(Err(e)) => {
            error!("Failed to execute zkEngine verify: {}", e);
            let _ = state.tx.send(WsMessage {
                msg_type: "message".to_string(),
                content: format!("Verification failed: {}", e),
                data: Some(json!({
                    "type": "verification_complete",
                    "proof_id": proof_id,
                    "is_valid": false,
                    "error": format!("Execution error: {}", e)
                })),
            });
        }
        Err(e) => {
            error!("Task join error during verification: {}", e);
            let _ = state.tx.send(WsMessage {
                msg_type: "message".to_string(),
                content: "Internal verification error".to_string(),
                data: Some(json!({
                    "type": "verification_complete", 
                    "proof_id": proof_id,
                    "is_valid": false,
                    "error": "Internal error"
                })),
            });
        }
    }
}

// FIXED: generate_real_proof function - remove duplicate messages
async fn generate_real_proof(
    state: AppState,
    proof_id: String,
    metadata: ProofMetadata,
    args: Vec<String>,
) {
    let start_time = Instant::now();
    
    // Update status to running (NO WebSocket message here - already sent)
    {
        let mut proofs = state.proof_store.lock().await;
        if let Some(proof) = proofs.get_mut(&proof_id) {
            proof.status = ProofStatus::Running;
        }
        // Save to disk
        if let Err(e) = save_proofs_to_disk(&*proofs).await {
            error!("Failed to save proofs to disk: {}", e);
        }
    }
    
    // Create proof directory
    let proof_dir = format!("{}/{}", state.proofs_dir, proof_id);
    fs::create_dir_all(&proof_dir).ok();
    
    // Check if WASM file exists
    if !Path::new(&metadata.wasm_path).exists() {
        error!("WASM file not found: {}", metadata.wasm_path);
        update_proof_failed(&state, &proof_id, "WASM file not found").await;
        return;
    }
    
    // Clone values needed inside the closure
    let zkengine_binary = state.zkengine_binary.clone();
    let wasm_path = metadata.wasm_path.clone();
    let step_size = metadata.step_size;
    let proof_dir_clone = proof_dir.clone();
    let args_vec: Vec<String> = args.clone();
    
    info!("Running zkEngine command for proof {}", proof_id);
    
    match tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&zkengine_binary);
        cmd.arg("prove")
            .arg("--wasm").arg(&wasm_path)
            .arg("--step").arg(step_size.to_string())
            .arg("--out-dir").arg(&proof_dir_clone);
        
        // Add arguments
        for arg in args_vec {
            cmd.arg(arg);
        }
        
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped());
        
        info!("Executing command: {:?}", cmd);
        cmd.output()
    }).await {
        Ok(Ok(output)) => {
            let duration = start_time.elapsed();
            
            if output.status.success() {
                // Find the generated proof file
                if let Ok(entries) = fs::read_dir(&proof_dir) {
                    for entry in entries.filter_map(Result::ok) {
                        let path = entry.path();
                        if path.extension().and_then(|s| s.to_str()) == Some("bin") {
                            // Calculate metrics
                            let file_size = fs::metadata(&path)
                                .map(|m| m.len() as f64 / 1_048_576.0)
                                .unwrap_or(0.0);
                            
                            let file_hash = calculate_file_hash(&path).await;
                            
                            // Update proof record
                            let mut proofs = state.proof_store.lock().await;
                            if let Some(proof) = proofs.get_mut(&proof_id) {
                                proof.status = ProofStatus::Complete;
                                proof.file_path = Some(path.to_string_lossy().to_string());
                                proof.metrics = ProofMetrics {
                                    generation_time_secs: duration.as_secs_f64(),
                                    file_size_mb: file_size,
                                    file_hash: file_hash.clone(),
                                    peak_memory_mb: None,
                                };
                            }
                            
                            // Save to disk
                            if let Err(e) = save_proofs_to_disk(&*proofs).await {
                                error!("Failed to save proofs to disk: {}", e);
                            }
                            
                            // Send SINGLE success message
                            let _ = state.tx.send(WsMessage {
                                msg_type: "message".to_string(),
                                content: format!(
                                    "Proof generated successfully! ID: {} Time: {:.1}s Size: {:.1}MB",
                                    &proof_id[..8],
                                    duration.as_secs_f64(),
                                    file_size
                                ),
                                data: Some(json!({ 
                                    "type": "proof_complete",
                                    "proof_id": proof_id,
                                    "status": "complete",
                                    "function": metadata.function,
                                    "arguments": metadata.arguments,
                                    "step_size": metadata.step_size,
                                    "time": duration.as_secs_f64(),
                                    "size": file_size,
                                    "hash": file_hash.clone()
                                })),
                            });
                            
                            return;
                        }
                    }
                }
                
                // No proof file found
                update_proof_failed(&state, &proof_id, "Proof file not found after generation").await;
            } else {
                let error = String::from_utf8_lossy(&output.stderr);
                error!("zkEngine command failed: {}", error);
                update_proof_failed(&state, &proof_id, &format!("zkEngine error: {}", error)).await;
            }
        }
        Ok(Err(e)) => {
            error!("Failed to execute zkEngine: {}", e);
            update_proof_failed(&state, &proof_id, &format!("Execution error: {}", e)).await;
        }
        Err(e) => {
            error!("Task join error: {}", e);
            update_proof_failed(&state, &proof_id, "Internal error").await;
        }
    }
}

// FIXED: update_proof_failed function
async fn update_proof_failed(state: &AppState, proof_id: &str, error: &str) {
    let mut proofs = state.proof_store.lock().await;
    if let Some(proof) = proofs.get_mut(proof_id) {
        proof.status = ProofStatus::Failed(error.to_string());
    }
    
    // Save to disk
    if let Err(e) = save_proofs_to_disk(&*proofs).await {
        error!("Failed to save proofs to disk: {}", e);
    }
    
    let _ = state.tx.send(WsMessage {
        msg_type: "message".to_string(),
        content: format!("Proof generation failed: {}", error),
        data: Some(json!({ 
            "type": "proof_failed",
            "proof_id": proof_id, 
            "error": error 
        })),
    });
}

async fn calculate_file_hash(path: &Path) -> String {
    match tokio::fs::read(path).await {
        Ok(contents) => {
            let mut hasher = Sha256::new();
            hasher.update(&contents);
            format!("{:x}", hasher.finalize())
        }
        Err(_) => "error".to_string(),
    }
}
