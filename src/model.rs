use std::path::PathBuf; 
use std::time::Duration; 

#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,              
    pub title: String,              
    pub artist: String,             
    pub album: String,              
    pub album_key: String,          
    pub duration: Duration,         
    pub disc: u32,                  
    pub number: u32,                
    pub lyrics: Option<String>,     
    pub cover: Option<Vec<u8>>,     
    pub netease_id: Option<String>, 
    pub qqmid: Option<String>,      
}

impl Track {
    pub fn display_artist(&self) -> &str {
        if self.artist.is_empty() {
            "未知艺术家" 
        } else {
            &self.artist 
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSource {
    #[default]
    Netease,
    Qq,
    Tidal,
}

impl SearchSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Netease => "网易云",
            Self::Qq => "QQ音乐",
            Self::Tidal => "Tidal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub name: String,               
    pub artist: String,             
    pub album: String,              
    pub source: SearchSource,       
    pub netease_id: Option<String>, 
    pub qqmid: Option<String>,      
    pub qq_songid: Option<String>,  
    pub albummid: Option<String>,   
    pub tidal_id: Option<String>,   
    pub quality: Option<String>,    
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayMode {
    #[default] 
    Sequential, 
    RepeatOne, 
    Shuffle,   
}

impl PlayMode {
    pub fn next(self) -> Self {
        match self {
            Self::Sequential => Self::RepeatOne,
            Self::RepeatOne => Self::Shuffle,
            Self::Shuffle => Self::Sequential,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Sequential => "顺序",
            Self::RepeatOne => "单曲循环",
            Self::Shuffle => "随机",
        }
    }
}
