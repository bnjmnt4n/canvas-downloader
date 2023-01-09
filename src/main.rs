use anyhow::{Context, Result};
use canvas::ProcessOptions;
use chrono::DateTime;
use clap::Parser;
use futures::{future::BoxFuture, FutureExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::header;
use std::{sync::{Arc, atomic::{AtomicUsize, Ordering}}, path::PathBuf};
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    let args = CommandLineOptions::parse();

    if (args.canvas_url.is_none() || args.canvas_token.is_none()) && args.canvas_credential_path.is_none() {
        panic!("Provide canvas url and token via -u and -t respectively or via a credential file -c");
    }

    if !args.destination_folder.exists() {
        std::fs::create_dir(&args.destination_folder)
            .with_context(|| format!("Failed to create directory: {}", args.destination_folder.to_string_lossy()))?;
    }

    let credentials: Option<canvas::Credentials> = if args.canvas_credential_path.is_some() {
        let path = args.canvas_credential_path.clone().unwrap();

        if !path.exists() {
            if !args.save_credentials {
                panic!("The given path to the credentials file does not exists");
            } else {
                Option::None
            }
        } else {
            if !args.save_credentials {
                let file = std::fs::File::open(path)?;
                serde_json::from_reader(file).expect("Credential file is not valid json")
            } else {
                Option::None
            }
        }
    } else {
        Option::None
    };

    let canvas_url = if args.canvas_url.is_some() {
        args.canvas_url.unwrap()
    } else {
        credentials.clone().unwrap().canvas_url
    };

    let canvas_token = if args.canvas_token.is_some() {
        args.canvas_token.unwrap()
    } else {
        credentials.clone().unwrap().canvas_token
    };

    if args.save_credentials {
        if args.canvas_credential_path.is_none() {
            panic!("Provide the destination path to save the credential to");
        }

        let path = args.canvas_credential_path.clone().unwrap();
        let file = std::fs::File::create(path)?;
        let credentials = canvas::Credentials {
            canvas_url: canvas_url.clone(),
            canvas_token: canvas_token.clone(),
        };
        serde_json::to_writer_pretty(file, &credentials)?;
    }

    let courses_link = format!("{}/api/v1/courses", canvas_url);

    let client = reqwest::Client::new();

    // do not directly deserialize into canvas::Course objects
    // there are may be courses that are restricted and not contain the fields needed to deserialise
    let courses_json = client.get(&courses_link)
        .bearer_auth(&canvas_token)
        .send()
        .await
        .with_context(|| format!("Something went wrong when reaching {}", &courses_link))?
        .json::<serde_json::Value>()
        .await?;

    let mut courses = vec![];
    for course_json in courses_json.as_array().unwrap() {
        if course_json.get("enrollments").is_some() {
            let course: canvas::Course = serde_json::from_value(course_json.to_owned()).unwrap();
            courses.push(course);
        }
    }

    let options = ProcessOptions {
        canvas_token: canvas_token.clone(),
        link: String::from(""),
        parent_folder_path: PathBuf::new(),
        client: client.clone(),
        files_to_download: Arc::new(Mutex::new(Vec::new())),
        download_newer: args.download_newer,
    };

    println!("Courses found:");
    for course in courses {
        println!("  * {} - {}", course.course_code, course.name);

        let course_folder_path = args
            .destination_folder
            .join(course.course_code.replace("/", "_"));
        if !course_folder_path.exists() {
            std::fs::create_dir(&course_folder_path).with_context(|| {
                format!(
                    "Failed to create directory: {}",
                    course_folder_path.to_string_lossy()
                )
            })?;
        }

        // this api gives us the root folder
        let course_folders_link = format!("{}/{}/folders/by_path/", &courses_link, course.id);

        let mut new_options = options.clone();
        new_options.link = course_folders_link;
        new_options.parent_folder_path = course_folder_path;

        process_folders(new_options).await;
    }

    println!("");

    // Tokio uses the number of cpus as num of work threads in the default runtime
    let num_worker_threads = num_cpus::get();
    let files_to_download = Arc::new(Arc::try_unwrap(options.files_to_download).unwrap().into_inner());
    let num_worker_extra_work = files_to_download.len() % num_worker_threads;
    let min_work = files_to_download.len() / num_worker_threads;
    let progress_bars = Arc::new(MultiProgress::new());

    println!("Downloading {} file{}", files_to_download.len(), if files_to_download.len() == 1 { "" } else { "s" } );

    let mut join_handles = Vec::new();
    let atomic_file_index = Arc::new(AtomicUsize::new(0));

    // We manually limit each worker thread to only deal with 1 file at all time to avoid
    // spamming http requests
    for i in 0..num_worker_threads {
        let mut work = min_work;
        if i < num_worker_extra_work {
            work += 1;
        }
        let canvas_token = canvas_token.clone();
        let client = client.clone();
        let files_to_download = files_to_download.clone();
        let progress_bars = progress_bars.clone();
        let atomic_file_index = atomic_file_index.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..work {
                let file_index = atomic_file_index.fetch_add(1, Ordering::Relaxed);
                let canvas_file = files_to_download.get(file_index).unwrap();

                // We need to determine the file size before we download, so we can create a ProgressBar
                // A Header request for the CONTENT_LENGTH header gets us the file size
                let download_size = {
                    let resp = client.head(&canvas_file.url).send().await.unwrap();
                    if resp.status().is_success() {
                        resp.headers() // Gives us the HeaderMap
                            .get(header::CONTENT_LENGTH) // Gives us an Option containing the HeaderValue
                            .and_then(|ct_len| ct_len.to_str().ok()) // Unwraps the Option as &str
                            .and_then(|ct_len| ct_len.parse().ok()) // Parses the Option as u64
                            .unwrap_or(0) // Fallback to 0
                    } else {
                        // We return an Error if something goes wrong here
                        println!("Failed to download {}", canvas_file.display_name);
                        continue
                    }
                };

                let progress_bar = progress_bars.add(ProgressBar::new(download_size));

                let mut style_template = "[{bar:20.cyan/blue}] {bytes}/{total_bytes} - {bytes_per_sec} - {msg}";
                termsize::get().map(|size| {
                    // arbitrary 100
                    if size.cols < 100 {
                        style_template = "[{wide_bar:.cyan/blue}] {total_bytes} - {msg}";
                    }
                });
                progress_bar.set_style(
                    ProgressStyle::default_bar()
                        .template(style_template).unwrap()
                        .progress_chars("=>-")
                );

                let message = format!("{}", canvas_file.display_name);

                progress_bar.set_message(message);

                let mut file = std::fs::File::create(&canvas_file.filepath).unwrap();
                // canvas also provides a modified_time of the file but updated_at should be more proper
                // as it probably represents the upload date of the file which is more apt for determining
                // if the file was changed since downloading it
                match DateTime::parse_from_rfc3339(&canvas_file.updated_at) {
                    Ok(updated_at) => {
                        match filetime::set_file_mtime(
                            &canvas_file.filepath,
                            filetime::FileTime::from_unix_time(
                                updated_at.timestamp(),
                                updated_at.timestamp_subsec_nanos())) {
                            Err(_) => {
                                println!("Failed to set modified time of {} with updated_at of {}", canvas_file.display_name, canvas_file.updated_at);
                            },
                            _ => {}
                        };
                    },
                    Err(_) => {
                        println!("Failed to parse updated_at time for {}, {}", canvas_file.display_name, canvas_file.updated_at);
                        continue;
                    }
                };

                let mut file_response = client.get(&canvas_file.url)
                    .bearer_auth(&canvas_token)
                    .send()
                    .await
                    .with_context(|| format!("Something went wrong when reaching {}", &canvas_file.url)).unwrap();

                while let Some(chunk) = file_response.chunk().await.unwrap() {
                    progress_bar.inc(chunk.len() as u64);
                    let mut cursor = std::io::Cursor::new(chunk);
                    std::io::copy(&mut cursor, &mut file).unwrap();
                }
                progress_bar.finish();
            }
        });
        
        join_handles.push(handle);
    }

    for handle in join_handles {
        handle.await?;
    }

    for canvas_file in Arc::try_unwrap(files_to_download).unwrap() {
        println!("Downloaded {} to {}", canvas_file.display_name, canvas_file.filepath.to_string_lossy());
    }

    Ok(())
}

// async recursion needs boxing
fn process_folders(options: ProcessOptions) -> BoxFuture<'static, ()> {
    async move {
        let canvas_token = &options.canvas_token;
        let folders_result = options.client.get(&options.link)
            .bearer_auth(&canvas_token)
            .send()
            .await
            .with_context(|| format!("Something went wrong when reaching {}", &options.link)).unwrap()
            .json::<canvas::FolderResult>()
            .await;
        
        match folders_result {
            Ok(canvas::FolderResult::Ok(folders)) => {
                for folder in folders {
                    // println!("  * {} - {}", folder.id, folder.name);
                    let sanitized_folder_name = sanitize_filename::sanitize(folder.name);
                    // if the folder has no parent, it is the root folder of a course
                    // so we avoid the extra directory nesting by not appending the root folder name
                    let folder_path = if folder.parent_folder_id.is_some() {
                        options.parent_folder_path.clone().join(sanitized_folder_name)
                    } else {
                        options.parent_folder_path.clone()
                    };
                    if !folder_path.exists() {
                        std::fs::create_dir(&folder_path)
                            .with_context(|| format!("Failed to create directory: {}", folder_path.to_string_lossy())).unwrap();
                    }

                    let mut new_options = options.clone();
                    new_options.link = folder.files_url.clone();
                    new_options.parent_folder_path = folder_path.clone();
                    process_files(new_options).await;
        
                    let mut new_options = options.clone();
                    new_options.link = folder.folders_url.clone();
                    new_options.parent_folder_path = folder_path.clone();
                    process_folders(new_options).await;
                }
            },
            Ok(canvas::FolderResult::Err{status}) => {
                let course_has_no_folders = status == "unauthorized";
                if !course_has_no_folders {
                    println!("Failed to access folders at link:{}, path:{}, status:{}", options.link, options.parent_folder_path.to_string_lossy(), status);
                }
            },
            Err(e) => {
                println!("Failed to deserialize folders at link:{}, path:{}\n{:?}", &options.link, &options.parent_folder_path.to_string_lossy(), e);
            }
        }
    }.boxed()
}

async fn process_files(options: ProcessOptions) {
    let files_result = options.client.get(&options.link)
        .bearer_auth(&options.canvas_token)
        .send()
        .await
        .with_context(|| format!("Something went wrong when reaching {}", &options.link)).unwrap()
        .json::<canvas::FileResult>()
        .await;
    
    fn updated(filepath: &PathBuf, new_modified: &str) -> bool {
        (|| -> Result<bool> {
            let old_modified = std::fs::metadata(filepath)?.modified()?;
            let new_modified = std::time::SystemTime::from(DateTime::parse_from_rfc3339(new_modified)?);
            let updated = old_modified < new_modified;
            if updated {
                println!("Found update for {filepath:?}. Use -n to download updated files.");
            }
            Ok(updated)
        })().unwrap_or(false)
    }
    
    match files_result {
        Ok(canvas::FileResult::Ok(mut files)) => {
            for file in &mut files {
                let sanitized_filename = sanitize_filename::sanitize(&file.display_name);
                file.filepath = options.parent_folder_path.join(sanitized_filename);
            }
            
            // only download files that do not exist or are updated
            let mut filtered_files = files.into_iter()
            .filter(|f| !f.filepath.exists() || (updated(&f.filepath, &f.updated_at)) && options.download_newer)
            .collect::<Vec<canvas::File>>();
            
            let mut lock = options.files_to_download.lock().await;
            lock.append(&mut filtered_files);
        },
        Ok(canvas::FileResult::Err { status }) => {
            let course_has_no_files = status == "unauthorized";
            if !course_has_no_files {
                println!("Failed to access files at link:{}, path:{}, status:{}", options.link, options.parent_folder_path.to_string_lossy(), status);
            }
        }
        Err(e) => {
            println!("Failed to deserialize files at link:{}, path:{}\n{:?}", &options.link, &options.parent_folder_path.to_string_lossy(), e);
        }
    };
}

#[derive(Parser)]
struct CommandLineOptions {
    #[clap(short = 'u', long, forbid_empty_values = true)]
    canvas_url: Option<String>,
    #[clap(short = 't', long, forbid_empty_values = true)]
    canvas_token: Option<String>,
    #[clap(short = 'c', long, parse(from_os_str), forbid_empty_values = true)]
    canvas_credential_path: Option<std::path::PathBuf>,
    #[clap(short = 'd', long, parse(from_os_str), default_value = ".")]
    destination_folder: std::path::PathBuf,
    #[clap(short = 's', long, takes_value = false)]
    save_credentials: bool,
    #[clap(short = 'n', long, takes_value = false)]
    download_newer: bool,
}

mod canvas {
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Clone, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Credentials {
        pub canvas_url: String,
        pub canvas_token: String,
    }

    #[derive(Deserialize)]
    pub struct Course {
        pub id: u32,
        pub name: String,
        pub course_code: String,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum FolderResult {
        Err { status: String },
        Ok(Vec<Folder>),
    }
    
    #[derive(Deserialize)]
    pub struct Folder {
        pub id: u32,
        pub name: String,
        pub folders_url: String,
        pub files_url: String,
        pub for_submissions: bool,
        pub can_upload: bool,
        pub parent_folder_id: Option<u32>,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum FileResult {
        Err { status: String },
        Ok(Vec<File>),
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct File {
        pub id: u32,
        pub folder_id: u32,
        pub display_name: String,
        pub size: u64,
        pub url: String,
        pub updated_at: String,
        #[serde(skip)]
        pub filepath: std::path::PathBuf,
    }

    #[derive(Clone)]
    pub struct ProcessOptions {
        pub canvas_token: String,
        pub client: reqwest::Client,
        pub link: String,
        pub parent_folder_path: std::path::PathBuf,
        pub files_to_download: Arc<Mutex<Vec<File>>>,
        pub download_newer: bool,
    }
}
