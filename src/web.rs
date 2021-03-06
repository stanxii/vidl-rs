use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use askama::Template;
use lazy_static::lazy_static;
use log::{error, info};
use rouille::{router, Request, Response};
use serde_derive::Serialize;

use crate::common::VideoStatus;
use crate::config::Config;
use crate::db::{Channel, DBVideoInfo};
use crate::worker::WorkerPool;

#[derive(Clone)]
pub(crate) struct Image {
    pub(crate) data: Vec<u8>,
    pub(crate) content_type: String,
}

pub(crate) struct ImageCache {
    images: HashMap<String, Image>,
}

#[derive(Clone)]
enum ImageCacheResponse {
    Redirect(String),
    Image(Image),
}

impl ImageCache {
    fn new() -> Self {
        ImageCache {
            images: HashMap::new(),
        }
    }

    fn get(
        &mut self,
        url: String,
        worker: Arc<Mutex<crate::worker::WorkerPool>>,
    ) -> Result<ImageCacheResponse> {
        if self.images.contains_key(&url) {
            let cached = self.images.get(&url);
            Ok(ImageCacheResponse::Image((*cached.unwrap()).clone()))
        } else {
            let thready_url: String = url.clone();
            let pool = worker.lock().unwrap();
            pool.enqueue(crate::worker::WorkItem::ThumbnailCache(thready_url));

            Ok(ImageCacheResponse::Redirect(url.into()))
        }
    }

    pub(crate) fn add(&mut self, url: &str, img: Image) {
        self.images.insert(url.into(), img);
    }
}

lazy_static! {
    pub(crate) static ref IMG_CACHE: Mutex<ImageCache> = Mutex::new(ImageCache::new());
}

#[derive(Debug, Serialize)]
pub struct WebChannel {
    id: i64,
    chanid: String,
    service: String,
    title: String,
    icon: String,
}

impl From<Channel> for WebChannel {
    fn from(src: Channel) -> WebChannel {
        WebChannel {
            id: src.id,
            chanid: src.chanid,
            service: src.service.as_str().into(),
            title: src.title,
            icon: src.thumbnail,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WebChannelList {
    channels: Vec<WebChannel>,
}

impl From<Vec<Channel>> for WebChannelList {
    fn from(src: Vec<Channel>) -> WebChannelList {
        let channels: Vec<WebChannel> = src.into_iter().map(|p| p.into()).collect();
        WebChannelList { channels }
    }
}

#[derive(Debug, Serialize)]
pub struct WebVideoInfo<'a> {
    id: i64,
    video_id: String,
    url: String,
    title: String,
    description: String,
    thumbnail_url: String,
    published_at: String,
    status_class: String,
    channel: &'a WebChannel,
}

fn status_css_class(status: VideoStatus) -> String {
    match status {
        VideoStatus::New => "ytdl-new",
        VideoStatus::Queued => "ytdl-queued",
        VideoStatus::Downloading => "ytdl-downloading",
        VideoStatus::Grabbed => "ytdl-grabbed",
        VideoStatus::GrabError => "ytdl-graberror",
        VideoStatus::Ignore => "ytdl-ignore",
    }
    .into()
}

impl<'a> From<(DBVideoInfo, &'a WebChannel)> for WebVideoInfo<'a> {
    fn from(src: (DBVideoInfo, &'a WebChannel)) -> WebVideoInfo<'a> {
        let (src, chan) = src;
        WebVideoInfo {
            id: src.id,
            video_id: src.info.id,
            url: src.info.url,
            title: src.info.title,
            description: src.info.description,
            thumbnail_url: src.info.thumbnail_url,
            published_at: src.info.published_at.to_rfc3339(),
            status_class: status_css_class(src.status),
            channel: chan,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WebChannelVideos<'a> {
    videos: Vec<WebVideoInfo<'a>>,
}

#[derive(Template)]
#[template(path = "channel_list.html")]
struct ChannelListTemplate<'a> {
    chans: &'a WebChannelList,
}

fn page_chan_list() -> Result<Response> {
    let cfg = crate::config::Config::load();
    let db = crate::db::Database::open(&cfg)?;
    let chans = crate::db::list_channels(&db)?;
    let ret: WebChannelList = chans.into();

    let t = ChannelListTemplate { chans: &ret };

    let html = t.render()?;
    Ok(Response::html(html))
}

#[derive(Template)]
#[template(path = "video_list.html")]
struct VideoListTemplate<'a> {
    videos: &'a WebChannelVideos<'a>,
    page: i64,
}

fn page_list_videos(id: Option<i64>, page: i64) -> Result<Response> {
    let cfg = crate::config::Config::load();
    let db = crate::db::Database::open(&cfg)?;
    let (c, videos): (Option<Channel>, Vec<DBVideoInfo>) = if let Some(id) = id {
        let c = crate::db::Channel::get_by_sqlid(&db, id)?;
        let videos = c.all_videos(&db, 50, page)?;
        (Some(c), videos)
    } else {
        let videos = crate::db::all_videos(&db, 50, page)?;
        (None, videos)
    };

    // Construct a map of WebChannel's to be referenced by each video
    let mut chans: HashMap<i64, WebChannel> = HashMap::new();
    if let Some(c) = c {
        chans.insert(c.id, c.into());
    } else {
        for v in &videos {
            let c = v.channel(&db)?;
            chans.insert(c.id, c.into());
        }
    }

    // Each WebChannelVideo is VideoInfo plus a reference to the channel it belongs to
    let ret: WebChannelVideos = WebChannelVideos {
        videos: videos
            .into_iter()
            .map(|v| {
                let wc = &chans[&v.chanid];
                (v, wc).into()
            })
            .collect(),
    };

    let t = VideoListTemplate {
        videos: &ret,
        page: page,
    };
    let html = t.render()?;
    Ok(Response::html(html))
}

fn page_download_video(videoid: i64, workers: Arc<Mutex<WorkerPool>>) -> Result<Response> {
    let cfg = crate::config::Config::load();
    let db = crate::db::Database::open(&cfg)?;
    let v = crate::db::DBVideoInfo::get_by_sqlid(&db, videoid)?;

    // Mark video as queued
    v.set_status(&db, VideoStatus::Queued)?;

    // Then add it to the work queue
    {
        let w = workers.lock().unwrap();
        w.enqueue(crate::worker::WorkItem::Download(v));
    }
    Ok(Response::text("cool"))
}

enum ThumbnailType {
    Video,
    Channel,
}

fn page_thumbnail(
    id: i64,
    what: ThumbnailType,
    workers: Arc<Mutex<WorkerPool>>,
) -> Result<Response> {
    let cfg = crate::config::Config::load();
    let db = crate::db::Database::open(&cfg)?;

    let url = match what {
        ThumbnailType::Channel => {
            let chan = crate::db::Channel::get_by_sqlid(&db, id)?;
            chan.thumbnail
        }
        ThumbnailType::Video => {
            let vi = crate::db::DBVideoInfo::get_by_sqlid(&db, id)?;
            vi.info.thumbnail_url
        }
    };

    let image = {
        let mut ic = IMG_CACHE.lock().unwrap();
        let image = ic.get(url, workers)?;
        image.clone()
    };
    match image {
        ImageCacheResponse::Redirect(url) => Ok(Response::redirect_303(url)),
        ImageCacheResponse::Image(image) => Ok(Response::from_data(
            image.content_type.clone(),
            image.data.clone(),
        )),
    }
}

fn handle_response(request: &Request, workers: Arc<Mutex<WorkerPool>>) -> Response {
    if let Some(request) = request.remove_prefix("/static") {
        // Can do dynamic serving of files with:
        // return rouille::match_assets(&request, "static");

        let x = match request.url().as_ref() {
            "/popperjs_core_2.js" => Some((
                include_str!("../static/popperjs_core_2.js"),
                "application/javascript",
            )),
            "/pure-min.css" => Some((include_str!("../static/pure-min.css"), "text/css")),
            "/tippy_6.js" => Some((
                include_str!("../static/tippy_6.js"),
                "application/javascript",
            )),
            _ => None,
        };
        return match x {
            None => Response::text("404").with_status_code(404),
            Some((data, t)) => Response::from_data(t, data),
        };
    }

    let resp: Result<Response> = router!(request,
        (GET) ["/"] => {
            page_chan_list()
        },
        (GET) ["/channel/_all"] => {
            let page: i64 = request.get_param("page").and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
            page_list_videos(None, page)
        },
        (GET) ["/channel/{chanid}", chanid: i64] => {
            let page: i64 = request.get_param("page").and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
            page_list_videos(Some(chanid), page)
        },
        (GET) ["/download/{videoid}", videoid: i64] => {
            page_download_video(videoid, workers.clone())
        },
        (GET) ["/thumbnail/video/{id}", id: i64] => {
            page_thumbnail(id, ThumbnailType::Video, workers.clone())
        },
        (GET) ["/thumbnail/channel/{id}", id: i64] => {
            page_thumbnail(id, ThumbnailType::Channel, workers.clone())
        },
        // Default route
        _ => {
            Ok(Response::text("404 Not found").with_status_code(404))
        }
    );
    match resp {
        Ok(r) => r,
        Err(e) => Response::text(&format!("Internal service error: {:?}", e)).with_status_code(500),
    }
}

fn serve(workers: Arc<Mutex<WorkerPool>>) -> Result<()> {
    let cfg = Config::load();

    println!("yep");
    let addr = format!("{}:{}", cfg.web_host, cfg.web_port);
    let url = format!("http://{}", &addr);
    info!("Listening on {}", &url);
    let _p = std::process::Command::new("terminal-notifier")
        .args(&["-message", "web server started", "-open", &url])
        .spawn();
    let srv = rouille::Server::new(&addr, move |request| {
        handle_response(request, workers.clone())
    })
    .unwrap();

    let running = Arc::new(AtomicBool::new(true));

    while running.load(Ordering::SeqCst) {
        srv.poll_timeout(Duration::from_millis(100));
    }

    Ok(())
}

pub fn main() -> Result<()> {
    let workers = Arc::new(Mutex::new(crate::worker::WorkerPool::start()));

    let w = workers.clone();
    let web_thread = std::thread::spawn(|| serve(w));

    web_thread.join().unwrap()?;

    Ok(())
}
