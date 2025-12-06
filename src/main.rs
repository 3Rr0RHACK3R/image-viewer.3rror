use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;

// Image file extensions we support
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "avif", "webp", "tiff", "svg", "ico",
];

// Application state
#[derive(Clone)]
struct AppState {
    current_directory: Arc<RwLock<PathBuf>>,
}

// Directory entry for API responses
#[derive(Debug, Serialize)]
struct DirectoryEntry {
    name: String,
    path: String,
    is_dir: bool,
    is_image: bool,
}

// Directory listing response
#[derive(Debug, Serialize)]
struct DirectoryListing {
    current_path: String,
    parent_path: Option<String>,
    entries: Vec<DirectoryEntry>,
}

// Query parameters for file operations
#[derive(Debug, Deserialize)]
struct FilePathQuery {
    path: String,
}

// Rename request body
#[derive(Debug, Deserialize)]
struct RenameRequest {
    old_path: String,
    new_name: String,
}

/// Calculate SHA256 hash of a file for backup identification
fn calculate_file_hash(file_path: &Path) -> io::Result<String> {
    let mut file = File::open(file_path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    
    Ok(format!("{:x}", hasher.finalize()))
}

/// Create a backup of a file in .safety_net folder
fn create_backup(file_path: &Path) -> io::Result<()> {
    // Get the parent directory
    let parent_dir = file_path.parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "File has no parent directory"))?;
    
    // Create .safety_net directory if it doesn't exist
    let backup_dir = parent_dir.join(".safety_net");
    fs::create_dir_all(&backup_dir)?;
    
    // Calculate file hash to avoid duplicate backups
    let file_hash = calculate_file_hash(file_path)?;
    
    // Check backup index to avoid duplicates
    let index_path = backup_dir.join("index.txt");
    if index_path.exists() {
        let index_content = fs::read_to_string(&index_path)?;
        if index_content.lines().any(|line| line == file_hash) {
            // File already backed up
            return Ok(());
        }
    }
    
    // Generate backup filename
    let file_stem = file_path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    
    let file_extension = file_path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    
    let backup_filename = if file_extension.is_empty() {
        format!("{}_{}.bak", file_stem, &file_hash[..8])
    } else {
        format!("{}_{}.{}", file_stem, &file_hash[..8], file_extension)
    };
    
    let backup_path = backup_dir.join(backup_filename);
    
    // Copy file to backup location
    fs::copy(file_path, &backup_path)?;
    
    // Update backup index
    let mut index_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path)?;
    
    writeln!(index_file, "{}", file_hash)?;
    
    Ok(())
}

/// Check if a file is an image based on its extension
fn is_image_file(file_path: &Path) -> bool {
    file_path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext_lower = ext.to_lowercase();
            IMAGE_EXTENSIONS.contains(&ext_lower.as_str())
        })
        .unwrap_or(false)
}

/// List directory contents
async fn list_directory_handler(
    State(state): State<AppState>,
    Query(query): Query<FilePathQuery>,
) -> Result<Json<DirectoryListing>, StatusCode> {
    let path = PathBuf::from(&query.path);
    
    // Validate path
    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    
    if !path.is_dir() {
        return Err(StatusCode::BAD_REQUEST);
    }
    
    // Read directory
    let entries_result = fs::read_dir(&path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    let mut entries = Vec::new();
    
    for entry_result in entries_result {
        let entry = entry_result.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let entry_path = entry.path();
        
        // Skip hidden files (starting with .)
        if let Some(file_name) = entry_path.file_name() {
            if let Some(name_str) = file_name.to_str() {
                if name_str.starts_with('.') {
                    continue;
                }
                
                let is_directory = entry_path.is_dir();
                let is_image = !is_directory && is_image_file(&entry_path);
                
                // Only include directories and images
                if is_directory || is_image {
                    entries.push(DirectoryEntry {
                        name: name_str.to_string(),
                        path: entry_path.to_string_lossy().to_string(),
                        is_dir: is_directory,
                        is_image,
                    });
                }
            }
        }
    }
    
    // Sort entries: directories first, then alphabetically
    entries.sort_by(|a, b| {
        match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });
    
    // Get parent path
    let parent_path = path.parent()
        .map(|p| p.to_string_lossy().to_string());
    
    // Update application state
    *state.current_directory.write().await = path.clone();
    
    Ok(Json(DirectoryListing {
        current_path: path.to_string_lossy().to_string(),
        parent_path,
        entries,
    }))
}

/// Serve image files
async fn serve_image_handler(
    AxumPath(encoded_path): AxumPath<String>,
) -> Result<Response, StatusCode> {
    let decoded_path = urlencoding::decode(&encoded_path)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    
    let file_path = PathBuf::from(decoded_path.as_ref());
    
    // Validate file
    if !file_path.exists() || !file_path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    
    // Read file
    let file_content = fs::read(&file_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    // Determine content type from extension
    let content_type = match file_path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("svg") => "image/svg+xml",
        Some("tiff") | Some("tif") => "image/tiff",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    };
    
    Ok((
        [(header::CONTENT_TYPE, content_type)],
        file_content,
    ).into_response())
}

/// Delete a file (with backup)
async fn delete_file_handler(
    Query(query): Query<FilePathQuery>,
) -> Result<StatusCode, StatusCode> {
    let file_path = PathBuf::from(&query.path);
    
    // Validate file
    if !file_path.exists() || !file_path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    
    // Create backup
    if let Err(e) = create_backup(&file_path) {
        eprintln!("Warning: Failed to create backup: {}", e);
    }
    
    // Delete file
    fs::remove_file(&file_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    Ok(StatusCode::OK)
}

/// Rename a file (with backup)
async fn rename_file_handler(
    Json(request): Json<RenameRequest>,
) -> Result<StatusCode, StatusCode> {
    let old_path = PathBuf::from(&request.old_path);
    
    // Validate old file
    if !old_path.exists() || !old_path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    
    // Get parent directory
    let parent_dir = old_path.parent()
        .ok_or(StatusCode::BAD_REQUEST)?;
    
    // Create new path
    let new_path = parent_dir.join(&request.new_name);
    
    // Check if new file already exists
    if new_path.exists() {
        return Err(StatusCode::CONFLICT);
    }
    
    // Create backup of old file
    if let Err(e) = create_backup(&old_path) {
        eprintln!("Warning: Failed to create backup: {}", e);
    }
    
    // Rename file
    fs::rename(&old_path, &new_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    Ok(StatusCode::OK)
}

/// Serve the main HTML page
async fn root_handler() -> Html<&'static str> {
    Html(include_str!("../index.html"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize application state
    let app_state = AppState {
        current_directory: Arc::new(RwLock::new(PathBuf::from("."))),
    };
    
    // Create router
    let app = Router::new()
        .route("/", get(root_handler))
        .route("/api/list", get(list_directory_handler))
        .route("/image/*path", get(serve_image_handler))
        .route("/api/delete", post(delete_file_handler))
        .route("/api/rename", post(rename_file_handler))
        .with_state(app_state);
    
    // Bind and serve
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    
    println!("🚀 Pin Manager Server started successfully!");
    println!("📡 Server running at: http://127.0.0.1:3000");
    println!("💾 Backups will be saved to .safety_net folders");
    println!("🎨 Open the browser and start browsing!");
    println!("\nPress Ctrl+C to stop the server\n");
    
    // Try to open browser automatically
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(&["/c", "start", "http://127.0.0.1:3000"])
            .spawn();
    }
    
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("http://127.0.0.1:3000")
            .spawn();
    }
    
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg("http://127.0.0.1:3000")
            .spawn();
    }
    
    axum::serve(listener, app).await?;
    
    Ok(())
}