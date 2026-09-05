pub mod app; 
pub mod cli; 
pub mod des; 
pub mod library; 
pub mod lyrics; 
pub mod model; 
pub mod netease; 

pub fn home_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

pub mod netease_net; 
pub mod playback; 
pub mod qqmusic; 
pub mod state; 
pub mod tidal; 
pub mod ui; 
