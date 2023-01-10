#![deny(clippy::unwrap_used)]

use anyhow::{Context, Result};
use canvas::ProcessOptions;
use chrono::DateTime;
use clap::Parser;
use futures::{future::BoxFuture, FutureExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::header;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::{Mutex, Semaphore};

#[tokio::main]
async fn main() -> Result<()> {
    let args = CommandLineOptions::parse();

    let cred = parse_credentials(&args);

    // Create sub-folder if not exists
    if !args.destination_folder.exists() {
        std::fs::create_dir(&args.destination_folder)
            .unwrap_or_else(|e| panic!("Failed to create destination directory, err={e}"));
    }

    let client = reqwest::Client::new();
    let courses_link = format!("{}/api/v1/users/self/favorites/courses", cred.canvas_url);

    // Get courses
    let courses: Vec<canvas::Course> = client
        // GET request
        .get(&courses_link)
        .bearer_auth(&cred.canvas_token)
        .send()
        .await
        .unwrap_or_else(|e| panic!("Something went wrong when reaching {courses_link}, err={e}"))
        // Parse json
        .json::<Vec<serde_json::Value>>()
        .await?
        // Filter valid courses
        .into_iter()
        .filter_map(|course_json| {
            course_json.get("enrollments").and_then(|_| {
                serde_json::from_value(course_json.clone()).unwrap_or_else(|e| {
                    panic!("Could not parse course with json={course_json}, err={e}")
                })
            })
        })
        .collect();

    let options = Arc::new(ProcessOptions {
        canvas_token: cred.canvas_token.clone(),
        client: client.clone(),
        files_to_download: Mutex::new(Vec::new()),
        download_newer: args.download_newer,
        concurrent_reqs: Semaphore::new(num_cpus::get()),
        n_active_reqs: AtomicUsize::new(0),
    });

    println!("Courses found:");
    for course in courses {
        println!("  * {} - {}", course.course_code, course.name);

        let course_folder_path = args
            .destination_folder
            .join(course.course_code.replace('/', "_"));
        if !course_folder_path.exists() {
            std::fs::create_dir(&course_folder_path).with_context(|| {
                format!(
                    "Failed to create directory: {}",
                    course_folder_path.to_string_lossy()
                )
            })?;
        }

        // this api gives us the root folder
        let course_folders_link = format!(
            "{}/api/v1/courses/{}/folders/by_path/",
            cred.canvas_url, course.id
        );

        process_folders(options.clone(), course_folders_link, course_folder_path).await;
    }

    println!();

    // Tokio uses the number of cpus as num of work threads in the default runtime
    let num_worker_threads = num_cpus::get();
    let files_to_download = Arc::new(options.files_to_download.lock().await.clone());
    let num_worker_extra_work = files_to_download.len() % num_worker_threads;
    let min_work = files_to_download.len() / num_worker_threads;
    let progress_bars = Arc::new(MultiProgress::new());

    println!(
        "Downloading {} file{}",
        files_to_download.len(),
        if files_to_download.len() == 1 {
            ""
        } else {
            "s"
        }
    );

    let mut join_handles = Vec::new();
    let atomic_file_index = Arc::new(AtomicUsize::new(0));

    // We manually limit each worker thread to only deal with 1 file at all time to avoid
    // spamming http requests
    for i in 0..num_worker_threads {
        let mut work = min_work;
        if i < num_worker_extra_work {
            work += 1;
        }
        let canvas_token = cred.canvas_token.clone();
        let client = client.clone();
        let files_to_download = files_to_download.clone();
        let progress_bars = progress_bars.clone();
        let atomic_file_index = atomic_file_index.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..work {
                let file_index = atomic_file_index.fetch_add(1, Ordering::Relaxed);
                let canvas_file = files_to_download.get(file_index).expect(
                    "Please report this issue on GitHub: downloading file with index out of bounds",
                );

                // We need to determine the file size before we download, so we can create a ProgressBar
                // A Header request for the CONTENT_LENGTH header gets us the file size
                let download_size = {
                    let resp = client
                        .head(&canvas_file.url)
                        .send()
                        .await
                        .unwrap_or_else(|e| {
                            panic!(
                                "Unable to get file information for {:?}, err={e}",
                                canvas_file
                            )
                        });
                    if resp.status().is_success() {
                        resp.headers() // Gives us the HeaderMap
                            .get(header::CONTENT_LENGTH) // Gives us an Option containing the HeaderValue
                            .and_then(|ct_len| ct_len.to_str().ok()) // Unwraps the Option as &str
                            .and_then(|ct_len| ct_len.parse().ok()) // Parses the Option as u64
                            .unwrap_or(0) // Fallback to 0
                    } else {
                        // We return an Error if something goes wrong here
                        println!("Failed to download {}", canvas_file.display_name);
                        continue;
                    }
                };

                let progress_bar = progress_bars.add(ProgressBar::new(download_size));

                let style_template = if termsize::get().map_or(false, |size| size.cols < 100) {
                    "[{wide_bar:.cyan/blue}] {total_bytes} - {msg}"
                } else {
                    "[{bar:20.cyan/blue}] {bytes}/{total_bytes} - {bytes_per_sec} - {msg}"
                };
                progress_bar.set_style(
                    ProgressStyle::default_bar()
                        .template(style_template)
                        .unwrap_or_else(|e| panic!("Please report this issue on GitHub: error with progress bar style={style_template}, err={e}"))
                        .progress_chars("=>-"),
                );

                let message = canvas_file.display_name.to_string();

                progress_bar.set_message(message);

                let mut file = std::fs::File::create(&canvas_file.filepath).unwrap_or_else(|e| {
                    panic!(
                        "Unable to create file={:?} with err={e}",
                        canvas_file.filepath
                    )
                });
                // canvas also provides a modified_time of the file but updated_at should be more proper
                // as it probably represents the upload date of the file which is more apt for determining
                // if the file was changed since downloading it
                match DateTime::parse_from_rfc3339(&canvas_file.updated_at) {
                    Ok(updated_at) => {
                        if filetime::set_file_mtime(
                            &canvas_file.filepath,
                            filetime::FileTime::from_unix_time(
                                updated_at.timestamp(),
                                updated_at.timestamp_subsec_nanos(),
                            ),
                        )
                        .is_err()
                        {
                            println!(
                                "Failed to set modified time of {} with updated_at of {}",
                                canvas_file.display_name, canvas_file.updated_at
                            );
                        };
                    }
                    Err(_) => {
                        println!(
                            "Failed to parse updated_at time for {}, {}",
                            canvas_file.display_name, canvas_file.updated_at
                        );
                        continue;
                    }
                };

                let mut file_response = client
                    .get(&canvas_file.url)
                    .bearer_auth(&canvas_token)
                    .send()
                    .await
                    .unwrap_or_else(|e| {
                        panic!(
                            "Something went wrong when reaching {}, err={e}",
                            canvas_file.url
                        )
                    });

                while let Some(chunk) = file_response.chunk().await.unwrap_or_else(|e| {
                    panic!(
                        "Something went wrong downloading {}, err={e}",
                        canvas_file.url
                    )
                }) {
                    progress_bar.inc(chunk.len() as u64);
                    let mut cursor = std::io::Cursor::new(chunk);
                    std::io::copy(&mut cursor, &mut file).unwrap_or_else(|e| {
                        panic!("Could not save file {:?}, err={e}", canvas_file.filepath)
                    });
                }
                progress_bar.finish();
            }
        });

        join_handles.push(handle);
    }

    for handle in join_handles {
        handle.await?;
    }

    for canvas_file in files_to_download.iter() {
        println!(
            "Downloaded {} to {}",
            canvas_file.display_name,
            canvas_file.filepath.to_string_lossy()
        );
    }

    Ok(())
}

fn parse_credentials(args: &CommandLineOptions) -> canvas::Credentials {
    // Parse...
    let cred = if let (Some(url), Some(token)) = (&args.canvas_url, &args.canvas_token) {
        // ...from CLI
        canvas::Credentials {
            canvas_url: url.clone(),
            canvas_token: token.clone(),
        }
    } else if let Some(path) = &args.canvas_credential_path {
        // ...from file
        assert!(
            !args.save_credentials,
            "To save credentials, please provide url and token via -u and -t respectively"
        );
        let file = std::fs::File::open(path)
            .unwrap_or_else(|e| panic!("Could not open credential file, err={e}"));
        serde_json::from_reader(file).expect("Credential file is not valid json")
    } else {
        panic!(
            "Provide canvas url and token via -u and -t respectively or via a credential file -c"
        );
    };

    // Save credentials
    if args.save_credentials {
        let Some(path) = &args.canvas_credential_path else {
            panic!("To save credentials, please provide credential file via -c");
        };
        let file = std::fs::File::create(path)
            .unwrap_or_else(|e| panic!("Could not open credential file, err={e}"));
        serde_json::to_writer_pretty(file, &cred)
            .unwrap_or_else(|e| panic!("Could not write credential file, err={e}"));
    }

    cred
}

// async recursion needs boxing
fn process_folders(
    options: Arc<ProcessOptions>,
    url: String,
    path: PathBuf,
) -> BoxFuture<'static, ()> {
    async move {
        let canvas_token = &options.canvas_token;
        let folders_result = options
            .client
            .get(&url)
            .bearer_auth(canvas_token)
            .send()
            .await
            .unwrap_or_else(|e| panic!("Something went wrong when reaching {url}, err={e}"))
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
                        path.clone().join(sanitized_folder_name)
                    } else {
                        path.clone()
                    };
                    if !folder_path.exists() {
                        std::fs::create_dir(&folder_path).unwrap_or_else(|e| {
                            panic!(
                                "Failed to create directory: {}, err={e}",
                                folder_path.to_string_lossy()
                            )
                        });
                    }

                    process_files(
                        options.clone(),
                        folder.files_url.clone(),
                        folder_path.clone(),
                    )
                    .await;

                    process_folders(
                        options.clone(),
                        folder.folders_url.clone(),
                        folder_path.clone(),
                    )
                    .await;
                }
            }
            Ok(canvas::FolderResult::Err { status }) => {
                let course_has_no_folders = status == "unauthorized";
                if !course_has_no_folders {
                    println!(
                        "Failed to access folders at link:{url}, path:{path:?}, status:{status}"
                    );
                }
            }
            Err(e) => {
                println!("Failed to deserialize folders at link:{url}, path:{path:?}\n{e:?}");
            }
        }
    }
    .boxed()
}

async fn process_files(options: Arc<ProcessOptions>, url: String, path: PathBuf) {
    let files_result = options
        .client
        .get(&url)
        .bearer_auth(&options.canvas_token)
        .send()
        .await
        .unwrap_or_else(|e| panic!("Something went wrong when reaching {url}, err={e}"))
        .json::<canvas::FileResult>()
        .await;

    fn updated(filepath: &PathBuf, new_modified: &str) -> bool {
        (|| -> Result<bool> {
            let old_modified = std::fs::metadata(filepath)?.modified()?;
            let new_modified =
                std::time::SystemTime::from(DateTime::parse_from_rfc3339(new_modified)?);
            let updated = old_modified < new_modified;
            if updated {
                println!("Found update for {filepath:?}. Use -n to download updated files.");
            }
            Ok(updated)
        })()
        .unwrap_or(false)
    }

    match files_result {
        Ok(canvas::FileResult::Ok(mut files)) => {
            for file in &mut files {
                let sanitized_filename = sanitize_filename::sanitize(&file.display_name);
                file.filepath = path.join(sanitized_filename);
            }

            // only download files that do not exist or are updated
            let mut filtered_files = files
                .into_iter()
                .filter(|f| {
                    !f.filepath.exists()
                        || (updated(&f.filepath, &f.updated_at)) && options.download_newer
                })
                .collect::<Vec<canvas::File>>();

            let mut lock = options.files_to_download.lock().await;
            lock.append(&mut filtered_files);
        }
        Ok(canvas::FileResult::Err { status }) => {
            let course_has_no_files = status == "unauthorized";
            if !course_has_no_files {
                println!("Failed to access files at link:{url}, path:{path:?}, status:{status}");
            }
        }
        Err(e) => {
            println!("Failed to deserialize files at link:{url}, path:{path:?}\n{e:?}");
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
    canvas_credential_path: Option<PathBuf>,
    #[clap(short = 'd', long, parse(from_os_str), default_value = ".")]
    destination_folder: PathBuf,
    #[clap(short = 's', long, takes_value = false)]
    save_credentials: bool,
    #[clap(short = 'n', long, takes_value = false)]
    download_newer: bool,
}

mod canvas {
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::AtomicUsize;
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

    pub struct ProcessOptions {
        pub canvas_token: String,
        pub client: reqwest::Client,
        pub download_newer: bool,
        pub files_to_download: Mutex<Vec<File>>,     // Output
        pub concurrent_reqs: tokio::sync::Semaphore, // Limit concurrent requests
        pub n_active_reqs: AtomicUsize,              // Main thread waits for this to be 0
    }
}
