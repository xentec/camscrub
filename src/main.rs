#![allow(unused_imports)]
#![allow(unused_variables)]

use std::{collections::BTreeSet, path::Path, str::FromStr};

use tokio::{
	runtime, time, fs, io,
	io::AsyncWriteExt,
};
use tokio_stream::StreamExt;
use futures_util::future::TryFutureExt;
use reqwest as http;

use anyhow::{self, Result, Context};
use structopt::{self, StructOpt};

use serde::{Deserialize, Serialize};
use chrono;

use indicatif::{self, ProgressIterator};


#[derive(StructOpt, Default)]
#[structopt(name = "camscrub")]
struct Opt
{
	#[structopt()]
	/// Base URL of the webcam site
	url: String,

	#[structopt(parse(from_os_str), default_value = ".")]
	/// Path to download folder
	download_dir: std::path::PathBuf,

	#[structopt(long("no-dl"))]
	/// Do not download, only print links
	list_only: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>>
{
	let opt = Opt::from_args();
	// let opt = Opt { url: "http://othcam.oth-regensburg.de/webcam/".into(), ..Default::default() };
	let rt = runtime::Builder::new_current_thread()
		.enable_all()
		.build()?;

	rt.block_on(run(opt))?;
	rt.shutdown_timeout(time::Duration::from_secs(10));
	Ok(())
}

async fn run(opt: Opt) -> Result<(), Box<dyn std::error::Error>>
{
	let client = http::Client::builder()
		.timeout(std::time::Duration::from_secs(10))
		.build()
		.context("failed to build http client")?;

	#[derive(Debug, Default, Serialize)]
	struct ListRequest {
		wc: String,
		img: String,
		thumbs: u32,
	}

	#[derive(Debug, Default, Deserialize)]
	struct ListResponse {
		image: String,
		thumbs: Vec<String>,
	}


	let list_req = ListRequest { wc: "Regensburg".into(), thumbs: 500, ..Default::default() };

	let pb = indicatif::ProgressBar::new(1)
		.with_style(indicatif::ProgressStyle::default_bar()
			.template("{msg} {wide_bar:.cyan/blue} {pos:>6}/{len:6} [{percent}% {elapsed_precise} --> {eta_precise}]")
			.progress_chars("##-"));
	pb.set_draw_rate(4);
	pb.println(format!("Fetching image URLs for {} @ {} ...", &list_req.wc, &opt.url));

	let mut img_list = BTreeSet::<String>::new();
	let mut img_oldest = String::new();
	let list_url = opt.url.clone() + "/include/list.php";
	let req_base = client.get(list_url)
		.query(&list_req);

	loop {
		let res = req_base.try_clone().unwrap()
			.query(&[("img", &img_oldest)])
			.send()
			.await.context("failed to send request")?
			.json::<ListResponse>()
			.await.context("failed to parse response")?;

		img_oldest = res.thumbs.first().cloned().unwrap_or_else(|| {
			pb.println(format!("Oldest image: {}", img_oldest));
			Default::default()
		});

		img_list.extend(res.thumbs.into_iter()
			.map(|img| img.strip_suffix("_la.jpg")
				.map(|s| s.to_owned()).unwrap_or(img))
			.filter(|img| img.ends_with("00")));

		if img_oldest.is_empty() {
			break;
		}

		pb.set_message(format!("new oldest: {}", img_oldest));
		pb.set_length(img_list.len() as u64);
	}

	if opt.list_only {
		let mut stdout = io::BufWriter::new(io::stdout());
		for img in img_list {
			stdout.write_all(img.as_bytes()).await?;
			stdout.write_all("\n".as_bytes()).await?;
		}
		stdout.flush().await?;
		return Ok(());
	}

	let img_list: Vec<_> = img_list.into_iter().rev().collect();
	//img_list.sort_by(|a, b| a.cmp(b));

	pb.println(format!("Downloading {} images...", img_list.len()));
	for img in img_list {
		pb.set_message(img.clone());
		match download_img(&client, &opt.download_dir, &http::Url::from_str(&opt.url)?, &list_req.wc, &img).await {
			Ok(_) => pb.inc(1),
			Err(err) => pb.println(format!("failed to download {}: {}", &img, err)),
		}
	}

	if pb.position() == pb.length() {
		pb.finish_with_message("Download complete!");
	} else {
		pb.finish_with_message("Download partially complete!");
	}

	Ok(())
}

async fn download_img(client: &http::Client, download_dir: &Path, base_url: &http::Url, webcam: &str, img: &str) -> Result<()>
{
	let filename = img.to_owned() + "_hu.jpg";
	let url = base_url.join(&(webcam.to_owned() + "/"))?
		.join(&filename)?;
	let response = client.get(url.clone())
		.send()
		.await.context("failed to send download request")?
		.error_for_status().with_context(|| format!("failed to download {}", &url))?;

	let path_file = download_dir.to_owned().join(&webcam).join(&filename);
	if let Some(len) = response.content_length() {
		if let Ok(md) = fs::metadata(&path_file).await {
			if md.len() == len {
				return Ok(());
			}
		}
	}

	let path_dir = path_file.parent().unwrap();
	fs::create_dir_all(&path_dir).await
		.with_context(|| format!("failed to create directory {}", path_dir.display()))?;

	let mut file = fs::File::create(&path_file).await
		.with_context(|| format!("failed to create image file {}", path_file.display()))?;

	let mut stream = response.bytes_stream();
	while let Some(chunk) = stream.next().await {
		file.write_all_buf(&mut chunk?).await
			.context("failed to write")?;
	}
	file.flush().await.context("failed to flush")?;

	Ok(())
}
