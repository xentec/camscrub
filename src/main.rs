#![allow(unused_imports)]
#![allow(unused_variables)]

use std::{
	collections::BTreeSet,
	path::{Path, PathBuf},
	str::FromStr, sync::Arc
};

use anyhow::{Result, Context};
use clap::Parser;
use indicatif::{ProgressBar, ProgressIterator};

use tokio::{
	runtime, time, fs, io,
	io::AsyncWriteExt,
};
use tokio_stream::StreamExt;
use futures_util::future::TryFutureExt;
use async_channel;

use reqwest as http;
use http::header::HeaderValue;

use nix::{sys::time::{TimeVal, TimeValLike}};

use serde::{Deserialize, Serialize};
use chrono::{prelude::*, format::Fixed};




#[derive(Parser, Debug)]
#[clap(about, version)]
struct Opt
{
	/// Path to download folder
	#[clap(parse(from_os_str), default_value = ".")]
	download_dir: PathBuf,

	/// Base URL of the webcam site
	#[clap(default_value = "http://othcam.oth-regensburg.de/webcam/Regensburg/")]
	url: http::Url,
}

fn main() -> Result<(), Box<dyn std::error::Error>>
{
	let opt = Opt::parse();
	if opt.url.cannot_be_a_base() {
		return Err("URL is not supported".into());
	}

	let rt = runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?;

	rt.block_on(run(opt))?;
	rt.shutdown_timeout(time::Duration::from_secs(10));
	Ok(())
}

async fn run(opt: Opt) -> Result<()>
{
	let pb = ProgressBar::new(0)
		.with_style(indicatif::ProgressStyle::default_bar()
			.template("{msg} {pos:>6}/{len:6} {elapsed_precise}")
			.progress_chars("##-"));
	pb.set_draw_rate(4);
	let pb = Arc::new(pb);

	let url_base = {
		let mut url = opt.url.clone();
		url.path_segments_mut().unwrap().pop_if_empty();
		url
	};

	let client = http::Client::builder()
		.timeout(time::Duration::from_secs(10))
		.build()
		.context("failed to build http client")?;

	let task_range = 0..4;
	let (img_tx, img_rx) = async_channel::bounded::<String>(64 * task_range.len());
	let mut tasks = Vec::new();

	for id in task_range.clone() {
		let img_rx = img_rx.clone();
		let pb = pb.clone();
		let client = client.clone();
		let url_base = url_base.clone();
		let download_dir = opt.download_dir.clone();

		let task = tokio::spawn(async move {
			while let Ok(img) = img_rx.recv().await {
				if img.is_empty() {
					break;
				}

				let img_path = img.clone() + "_hu.jpg";
				let url = {
					let mut url = url_base.clone();
					url.path_segments_mut().unwrap().extend(img_path.split('/'));
					url
				};
				let mut path = download_dir.clone();
				path.push(&img_path);

				match download(&client, &url, &path).await {
					Ok(v) => {
						pb.inc(1);
						let stat = match v {
							Status::Downloaded => pb.println(format!("loaded {} ...", img)),
							Status::Exists => (),
						};
					},
					Err(err) => pb.println(format!("failed to download {}: {}", &img_path, err)),
				}
			}
		});
		tasks.push(task);
	}

	#[derive(Serialize)]
	struct ListRequest {
		wc: String,
		thumbs: u32,
	}

	#[derive(Deserialize)]
	struct ListResponse {
		thumbs: Vec<String>,
	}

	let webcam = url_base.path_segments()
		.and_then(Iterator::last)
		.context("missing webcam at the end of URL")?;

	let url_list = {
		let mut url = url_base.clone();
		url.path_segments_mut().unwrap()
			.pop()
			.extend(["include", "list.php"]);
		url
	};
	let list_req = ListRequest { wc: webcam.to_owned(), thumbs: 500 };

	let mut img_oldest = String::new();
	let req_base = client
		.get(url_list)
		.query(&list_req);

	pb.println(format!("Searching image URLs in {} ...", &url_base));
	loop {
		let res = req_base.try_clone().unwrap()
			.query(&[("img", &img_oldest)])
			.send()
			.await.context("failed to send request")?
			.json::<ListResponse>()
			.await.context("failed to parse response")?;

		let img_urls: Vec<_> = res.thumbs.into_iter()
			.map(|img| img.strip_suffix("_la.jpg")
				.map(|s| s.to_owned()).unwrap_or(img))
			.collect();

		let img_count = img_urls.len();
		img_oldest = img_urls.first()
			.cloned()
			.unwrap_or_default();

		pb.set_message(format!("search & load... (oldest: {})", img_oldest));
		pb.inc_length(img_count as _);

		for img_url in img_urls.into_iter() {
			img_tx.send(img_url)
				.await.context("failed to distribute image URLs")?;
		}

		if img_oldest.is_empty() {
			pb.set_message(format!("loading... (oldest: {})", img_oldest));
			break;
		}
	}

	// Terminate tasks
	for id in task_range {
		img_tx.send(Default::default()).await.ok();
	}
	// ..and await their end
	for task in tasks {
		task.await.ok();
	}

	let end_msg = if pb.position() == pb.length() {
		"Download complete!"
	} else {
		"Download partially complete (some errors occurred)!"
	};
	pb.finish_with_message(end_msg);

	Ok(())
}

enum Status {
	Downloaded,
	Exists,
}

async fn download(client: &http::Client, url: &http::Url, path: &Path) -> Result<Status>
{
	let mtime = fs::metadata(path).await
		.and_then(|md| md.modified())
		.map(DateTime::<Local>::from)
		.unwrap_or(Local.timestamp(0, 0))
		.with_timezone(&Utc);

	let resp = client.get(url.clone())
		.header("If-Modified-Since", mtime.to_rfc2822())
		.send()
		.await.context("failed to send download request")?
		.error_for_status()
		.with_context(|| format!("failed to download {}", &url))?;

	if resp.status() == http::StatusCode::NOT_MODIFIED {
		return Ok(Status::Exists);
	}

	let mtime = resp.headers().get("Last-Modified")
		.context("missing Last-Modified header")
		.and_then(|hv|  hv.to_str().context("invalid header value"))
		.and_then(|value| DateTime::parse_from_rfc2822(&value)
				.map(|dt| dt.with_timezone(&Utc))
				.context("invalid modify time"))
		.unwrap_or_else(|_| Utc::now());

	let path_dir = path.parent().unwrap();
	fs::create_dir_all(&path_dir).await
		.with_context(|| format!("failed to create directory {}", path_dir.display()))?;

	let mut file = fs::File::create(path).await
		.with_context(|| format!("failed to create image file {}", path.display()))?;

	if let Some(len) = resp.content_length() {
		file.set_len(len).await.ok();
	}

	let mut stream = resp.bytes_stream();
	while let Some(chunk) = stream.next().await {
		file.write_all_buf(&mut chunk?).await
			.context("failed to write")?;
	}
	file.flush().await.context("failed to flush")?;

	// set modification date from server
	let tv = TimeVal::milliseconds(mtime.timestamp_millis());
	nix::sys::stat::utimes(path, &tv, &tv).ok();

	Ok(Status::Downloaded)
}
