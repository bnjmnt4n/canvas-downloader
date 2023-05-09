#![deny(clippy::unwrap_used)]

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::ops::Add;
use std::time::Duration;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use anyhow::{Context, Error, Result};
use chrono::{DateTime, Local};
use clap::Parser;
use futures::future::{ready, join_all};
use futures::{stream, StreamExt, TryStreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rand::Rng;
use regex::Regex;
use reqwest::{header, Response, Url};
use select::document::Document;
use select::predicate::Name;

use canvas::{File, ProcessOptions};

#[derive(Parser)]
#[command(name = "Canvas Downloader")]
#[command(version)]
struct CommandLineOptions {
    #[arg(short = 'c', long, value_name = "FILE")]
    credential_file: PathBuf,
    #[arg(short = 'd', long, value_name = "FOLDER", default_value = ".")]
    destination_folder: PathBuf,
    #[arg(short = 'n', long)]
    download_newer: bool,
    #[arg(short = 't', long, value_name = "ID", num_args(1..))]
    term_ids: Option<Vec<u32>>,
}

macro_rules! fork {
    // Motivation: recursive async functions are unsupported. We avoid this by using a non-async
    // function `f` to tokio::spawn our recursive function. Conveniently, we can wrap our barrier logic in this function
    ($f:expr, $arg:expr, $T:ty, $options:expr) => {{
        fn g(arg: $T, options: Arc<ProcessOptions>) {
            options.n_active_requests.fetch_add(1, Ordering::AcqRel);
            tokio::spawn(async move {
                let _sem = options.sem_requests.acquire().await.unwrap_or_else(|e| {
                    panic!("Please report on GitHub. Unexpected closed sem, err={e}")
                });
                let res = $f(arg, options.clone()).await;
                let new_val = options.n_active_requests.fetch_sub(1, Ordering::AcqRel) - 1;
                if new_val == 0 {
                    options.notify_main.notify_one();
                }
                if let Err(e) = res {
                    eprintln!("{e:?}");
                }
            });
        }
        g($arg, $options);
    }};
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CommandLineOptions::parse();

    // Load credentials
    let file = std::fs::File::open(&args.credential_file)
        .with_context(|| "Could not open credential file")?;
    let cred: canvas::Credentials =
        serde_json::from_reader(file).with_context(|| "Credential file is not valid json")?;

    // Create sub-folder if not exists
    if !args.destination_folder.exists() {
        std::fs::create_dir(&args.destination_folder)
            .unwrap_or_else(|e| panic!("Failed to create destination directory, err={e}"));
    }

    // Prepare GET request options
    let client = reqwest::ClientBuilder::new()
        .tcp_keepalive(Some(Duration::from_secs(10)))
        .http2_keep_alive_interval(Some(Duration::from_secs(2)))
        .build()
        .with_context(|| "Failed to create HTTP client")?;
    let courses_link = format!("{}/api/v1/users/self/favorites/courses", cred.canvas_url);
    let options = Arc::new(ProcessOptions {
        canvas_token: cred.canvas_token.clone(),
        canvas_url: cred.canvas_url.clone(),
        client: client.clone(),
        // Process
        files_to_download: tokio::sync::Mutex::new(Vec::new()),
        download_newer: args.download_newer,
        // Download
        progress_bars: MultiProgress::new(),
        progress_style: {
            let style_template = if termsize::get().map_or(false, |size| size.cols < 100) {
                "[{wide_bar:.cyan/blue}] {total_bytes} - {msg}"
            } else {
                "[{bar:20.cyan/blue}] {bytes}/{total_bytes} - {bytes_per_sec} - {msg}"
            };
            ProgressStyle::default_bar()
                .template(style_template)
                .unwrap_or_else(|e| panic!("Please report this issue on GitHub: error with progress bar style={style_template}, err={e}"))
                .progress_chars("=>-")
        },
        // Synchronization
        n_active_requests: AtomicUsize::new(0),
        sem_requests: tokio::sync::Semaphore::new(8), // WARN magic constant.
        notify_main: tokio::sync::Notify::new(),
        // TODO handle canvas rate limiting errors, maybe scale up if possible
    });

    // Get courses
    let courses: Vec<canvas::Course> = get_pages(courses_link.clone(), &options)
        .await?
        .into_iter()
        .map(|resp| resp.json::<Vec<serde_json::Value>>()) // resp --> Result<Vec<json>>
        .collect::<stream::FuturesUnordered<_>>() // (in any order)
        .flat_map_unordered(None, |json_res| {
            let jsons = json_res.unwrap_or_else(|e| panic!("Failed to parse courses, err={e}")); // Result<Vec<json>> --> Vec<json>
            stream::iter(jsons.into_iter()) // Vec<json> --> json
        })
        .filter(|json| ready(json.get("enrollments").is_some())) // (enrolled?)
        .map(serde_json::from_value) // json --> Result<course>
        .try_collect()
        .await
        .with_context(|| "Error when getting course json")?; // Result<course> --> course

    // Filter courses by term IDs
    let Some(term_ids) = args.term_ids else {
        println!("Please provide the Term ID(s) to download via -t");
        print_all_courses_by_term(&courses);
        return Ok(());
    };
    let courses_matching_term_ids: Vec<&canvas::Course> = courses
        .iter()
        .filter(|course_json| term_ids.contains(&course_json.enrollment_term_id))
        .collect();
    if courses_matching_term_ids.is_empty() {
        println!("Could not find any course matching Term ID(s) {term_ids:?}");
        println!("Please try the following ID(s) instead");
        print_all_courses_by_term(&courses);
        return Ok(());
    }

    println!("Courses found:");
    for course in courses_matching_term_ids {
        println!("  * {} - {}", course.course_code, course.name);

        // Prep path and mkdir -p
        let course_folder_path = args
            .destination_folder
            .join(course.course_code.replace('/', "_"));
        create_folder_if_not_exist(&course_folder_path)?;
        // Prep URL for course's root folder
        let course_folders_link = format!(
            "{}/api/v1/courses/{}/folders/by_path/",
            cred.canvas_url, course.id
        );

        let folder_path = course_folder_path.join("files");
        fork!(
            process_folders,
            (course_folders_link, folder_path),
            (String, PathBuf),
            options.clone()
        );

        let course_api_link = format!(
            "{}/api/v1/courses/{}",
            cred.canvas_url, course.id
        );
        fork!(
            process_data,
            (course_api_link, course_folder_path),
            (String, PathBuf),
            options.clone()
        );
        
    }

    // Invariants
    // 1. Barrier semantics:
    //    1. Initial: n_active_requests > 0 by +1 synchronously in fork!()
    //    2. Recursion: fork()'s func +1 for subtasks before -1 own task
    //    3. --> n_active_requests == 0 only after all tasks done
    //    4. --> main() progresses only after all files have been queried
    // 2. No starvation: forks are done acyclically, all tasks +1 and -1 exactly once
    // 3. Bounded concurrency: acquire or block on semaphore before request
    // 4. No busy wait: Last task will see that there are 0 active requests and notify main
    options.notify_main.notified().await;
    assert_eq!(options.n_active_requests.load(Ordering::Acquire), 0);
    println!();

    let files_to_download = options.files_to_download.lock().await;
    println!(
        "Downloading {} file{}",
        files_to_download.len(),
        if files_to_download.len() == 1 {
            ""
        } else {
            "s"
        }
    );

    // Download files
    options.n_active_requests.fetch_add(1, Ordering::AcqRel); // prevent notifying until all spawned
    for canvas_file in files_to_download.iter() {
        fork!(
            atomic_download_file,
            canvas_file.clone(),
            File,
            options.clone()
        );
    }

    // Wait for downloads
    let new_val = options.n_active_requests.fetch_sub(1, Ordering::AcqRel) - 1;
    if new_val == 0 {
        // notify if all finished immediately
        options.notify_main.notify_one();
    }
    options.notify_main.notified().await;
    // Sanity check: running tasks trying to acquire sem will panic
    options.sem_requests.close();
    assert_eq!(options.n_active_requests.load(Ordering::Acquire), 0);

    for canvas_file in files_to_download.iter() {
        println!(
            "Downloaded {} to {}",
            canvas_file.display_name,
            canvas_file.filepath.to_string_lossy()
        );
    }

    Ok(())
}

async fn atomic_download_file(file: File, options: Arc<ProcessOptions>) -> Result<()> {
    // Create tmp file from hash
    let mut tmp_path = file.filepath.clone();
    tmp_path.pop();
    let mut h = DefaultHasher::new();
    file.display_name.hash(&mut h);
    tmp_path.push(&h.finish().to_string().add(".tmp"));

    // Aborted download?
    if let Err(e) = download_file((&tmp_path, &file), options.clone()).await {
        if let Err(e) = std::fs::remove_file(&tmp_path) {
            eprintln!(
                "Failed to remove temporary file {tmp_path:?} for {}, err={e:?}",
                file.display_name
            );
        }
        return Err(e);
    }

    // Update file time
    let updated_at = DateTime::parse_from_rfc3339(&file.updated_at)?;
    let updated_time = filetime::FileTime::from_unix_time(
        updated_at.timestamp(),
        updated_at.timestamp_subsec_nanos(),
    );
    if let Err(e) = filetime::set_file_mtime(&tmp_path, updated_time) {
        eprintln!(
            "Failed to set modified time of {} with updated_at of {}, err={e:?}",
            file.display_name, file.updated_at
        )
    }

    // Atomically rename file, doesn't change mtime
    std::fs::rename(&tmp_path, &file.filepath)?;
    Ok(())
}

async fn download_file(
    (tmp_path, canvas_file): (&PathBuf, &File),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    // Get file
    let mut resp = options
        .client
        .get(&canvas_file.url)
        .bearer_auth(&options.canvas_token)
        .send()
        .await
        .with_context(|| format!("Something went wrong when reaching {}", canvas_file.url))?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!(
            "Failed to download {}, got {resp:?}",
            canvas_file.display_name
        )));
    }

    // Create + Open file
    let mut file = std::fs::File::create(tmp_path)
        .with_context(|| format!("Unable to create tmp file for {:?}", canvas_file.filepath))?;

    // Progress bar
    let download_size = resp
        .headers() // Gives us the HeaderMap
        .get(header::CONTENT_LENGTH) // Gives us an Option containing the HeaderValue
        .and_then(|ct_len| ct_len.to_str().ok()) // Unwraps the Option as &str
        .and_then(|ct_len| ct_len.parse().ok()) // Parses the Option as u64
        .unwrap_or(0); // Fallback to 0
    let progress_bar = options.progress_bars.add(ProgressBar::new(download_size));
    progress_bar.set_message(canvas_file.display_name.to_string());
    progress_bar.set_style(options.progress_style.clone());

    // Download
    while let Some(chunk) = resp.chunk().await? {
        progress_bar.inc(chunk.len() as u64);
        let mut cursor = std::io::Cursor::new(chunk);
        std::io::copy(&mut cursor, &mut file)
            .with_context(|| format!("Could not write to file {:?}", canvas_file.filepath))?;
    }

    progress_bar.finish();
    Ok(())
}

fn print_all_courses_by_term(courses: &[canvas::Course]) {
    let mut grouped_courses: HashMap<u32, Vec<&str>> = HashMap::new();

    for course in courses.iter() {
        let course_id: u32 = course.enrollment_term_id;
        grouped_courses
            .entry(course_id)
            .or_insert_with(Vec::new)
            .push(&course.course_code);
    }
    println!("{: <10}| {:?}", "Term IDs", "Courses");
    for (key, value) in &grouped_courses {
        println!("{: <10}| {:?}", key, value);
    }
}

fn create_folder_if_not_exist(folder_path: &PathBuf) -> Result<()> {
    if !folder_path.exists() {
        std::fs::create_dir(&folder_path).with_context(|| {
            format!(
                "Failed to create directory: {}",
                folder_path.to_string_lossy()
            )
        })?;
    }
    Ok(())
}

// async recursion needs boxing
async fn process_folders(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let pages = get_pages(url, &options).await?;

    // For each page
    for pg in pages {
        let uri = pg.url().to_string();
        let folders_result = pg.json::<canvas::FolderResult>().await;

        match folders_result {
            // Got folders
            Ok(canvas::FolderResult::Ok(folders)) => {
                for folder in folders {
                    // println!("  * {} - {}", folder.id, folder.name);
                    let sanitized_folder_name = sanitize_filename::sanitize(folder.name);
                    // if the folder has no parent, it is the root folder of a course
                    // so we avoid the extra directory nesting by not appending the root folder name
                    let folder_path = if folder.parent_folder_id.is_some() {
                        path.join(sanitized_folder_name)
                    } else {
                        path.clone()
                    };
                    if !folder_path.exists() {
                        if let Err(e) = std::fs::create_dir(&folder_path) {
                            eprintln!(
                                "Failed to create directory: {}, err={e}",
                                folder_path.to_string_lossy()
                            );
                            continue;
                        };
                    }

                    fork!(
                        process_files,
                        (folder.files_url, folder_path.clone()),
                        (String, PathBuf),
                        options.clone()
                    );
                    fork!(
                        process_folders,
                        (folder.folders_url, folder_path),
                        (String, PathBuf),
                        options.clone()
                    );
                }
            }

            // Got status code
            Ok(canvas::FolderResult::Err { status }) => {
                let course_has_no_folders = status == "unauthorized";
                if !course_has_no_folders {
                    eprintln!(
                        "Failed to access folders at link:{uri}, path:{path:?}, status:{status}",
                    );
                }
            }

            // Parse error
            Err(e) => {
                eprintln!("Error when getting folders at link:{uri}, path:{path:?}\n{e:?}",);
            }
        }
    }

    Ok(())
}

async fn process_data(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let users_path = path.join("users.json");
    fork!(
        process_users,
        (url.clone(), users_path),
        (String, PathBuf),
        options.clone()
    );
    let discussions_path = path.join("discussions");
    create_folder_if_not_exist(&discussions_path)?;
    fork!(
        process_discussions,
        (url.clone(), false, discussions_path),
        (String, bool, PathBuf),
        options.clone()
    );
    let announcements_path = path.join("announcements");
    create_folder_if_not_exist(&announcements_path)?;
    fork!(
        process_discussions,
        (url.clone(), true, announcements_path),
        (String, bool, PathBuf),
        options.clone()
    );
    let pages_path = path.join("pages");
    create_folder_if_not_exist(&pages_path)?;
    fork!(
        process_pages,
        (url.clone(), pages_path),
        (String, PathBuf),
        options.clone()
    );
    Ok(())
}

async fn process_pages(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let pages_url = format!("{}pages", url);
    let pages = get_pages(pages_url, &options).await?;
    
    let pages_path = path.join("pages.json");
    let mut pages_file = std::fs::File::create(pages_path.clone())
        .with_context(|| format!("Unable to create file for {:?}", pages_path))?;

    for pg in pages {
        let uri = pg.url().to_string();
        let page_body = pg.text().await?;

        pages_file
            .write_all(page_body.as_bytes())
            .with_context(|| format!("Could not write to file {:?}", pages_path))?;

        let page_result = serde_json::from_str::<canvas::PageResult>(&page_body);

        match page_result {
            Ok(canvas::PageResult::Ok(pages)) => {
                for page in pages {
                    let page_url = format!("{}pages/{}", url, page.url);
                    let page_file_path = path.join(page.url.clone());
                    create_folder_if_not_exist(&page_file_path)?;
                    fork!(
                        process_page_body,
                        (page_url, page.url, page_file_path),
                        (String, String, PathBuf),
                        options.clone()
                    )
                }
            }

            Ok(canvas::PageResult::Err { status }) => {
                eprintln!("No pages found for url {} status: {}", uri, status);
            }

            Err(e) => {
                eprintln!("No pages found for url {} error: {}", uri, e);
            }
        };
    }

    Ok(())
}

async fn process_page_body(
    (url, title, path): (String, String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let page_resp = get_canvas_api(url.clone(), &options).await?;

    let page_file_path = path.join(format!("{}.json", title));
    let mut page_file = std::fs::File::create(page_file_path.clone())
        .with_context(|| format!("Unable to create file for {:?}", page_file_path))?;

    let page_resp_text = page_resp.text().await?;
    page_file
        .write_all(page_resp_text.as_bytes())
        .with_context(|| format!("Could not write to file {:?}", page_file_path))?;

    let page_body_result = serde_json::from_str::<canvas::PageBody>(&page_resp_text);
    match page_body_result {
        Result::Ok(page_body) => {
            let page_html = format!(
                "<html><head><title>{}</title></head><body>{}</body></html>",
                page_body.title, page_body.body);
            
            let page_html_path = path.join(format!("{}.html", page_body.url));
            let mut page_html_file = std::fs::File::create(page_html_path.clone())
                .with_context(|| format!("Unable to create file for {:?}", page_html_path))?;

            page_html_file
                .write_all(page_html.as_bytes())
                .with_context(|| format!("Could not write to file {:?}", page_html_path))?;
            
            fork!(
                process_html_links,
                (page_html, path),
                (String, PathBuf),
                options.clone()
            )
        }
        Result::Err(e) => {
            eprintln!("Error when parsing page body at link:{url}, path:{page_file_path:?}\n{e:?}",);
        }
    }
    Ok(())
}

async fn process_users (
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let users_url = format!("{}users?include_inactive=true&include[]=avatar_url&include[]=enrollments&include[]=email&include[]=observed_users&include[]=can_be_removed&include[]=custom_links", url);
    let pages = get_pages(users_url, &options).await?;
    
    let users_path = path.to_string_lossy();
    let mut users_file = std::fs::File::create(path.clone())
        .with_context(|| format!("Unable to create file for {:?}", users_path))?;

    for pg in pages {
        let page_body = pg.text().await?;
        
        users_file
            .write_all(page_body.as_bytes())
            .with_context(|| format!("Unable to write to file for {:?}", users_path))?;
    }

    Ok(())
}

async fn process_discussions(
    (url, announcement, path): (String, bool, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let discussion_url = format!("{}discussion_topics{}", url, if announcement { "?only_announcements=true" } else { "" });
    let pages = get_pages(discussion_url, &options).await?;

    let discussion_path = path.join("discussions.json");
    let mut discussion_file = std::fs::File::create(discussion_path.clone())
        .with_context(|| format!("Unable to create file for {:?}", discussion_path))?;

    for pg in pages {
        let uri = pg.url().to_string();
        let page_body = pg.text().await?;

        discussion_file
            .write_all(page_body.as_bytes())
            .with_context(|| format!("Unable to write to file for {:?}", discussion_path))?;

        let discussion_result = serde_json::from_str::<canvas::DiscussionResult>(&page_body);

        match discussion_result {
            Ok(canvas::DiscussionResult::Ok(discussions)) => {
                for discussion in discussions {
                    // download attachments
                    let discussion_path = path.join(format!("{}_{}", discussion.id, sanitize_filename::sanitize(discussion.title)));
                    create_folder_if_not_exist(&discussion_path)?;

                    let files = discussion.attachments
                        .into_iter()
                        .map(|mut f| {
                            f.display_name = format!("{}_{}", f.id, &f.display_name);
                            f
                        })
                        .collect();
                    {
                        let mut filtered_files = filter_files(&options, &path, files);
                        let mut lock = options.files_to_download.lock().await;
                        lock.append(&mut filtered_files);
                    }
                    
                    fork!(
                        process_html_links,
                        (discussion.message, discussion_path.clone()),
                        (String, PathBuf),
                        options.clone()
                    );
                    let view_url = format!("{}discussion_topics/{}/view", url, discussion.id);
                    fork!(
                        process_discussion_view,
                        (view_url, discussion_path),
                        (String, PathBuf),
                        options.clone()
                    )
                }
            }
            Ok(canvas::DiscussionResult::Err { status }) => {
                eprintln!(
                    "Failed to access discussions at link:{uri}, path:{path:?}, status:{status}",
                );
            }
            Err(e) => {
                eprintln!("Error when getting discussions at link:{uri}, path:{path:?}\n{e:?}",);
            }
        }
    }
    Ok(())
}

async fn process_discussion_view(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let resp = get_canvas_api(url.clone(), &options).await?;
    let discussion_view_body = resp.text().await?;
    
    let discussion_view_json = path.join("discussion.json");
    let mut discussion_view_file = std::fs::File::create(discussion_view_json.clone())
        .with_context(|| format!("Unable to create file for {:?}", discussion_view_json))?;

    discussion_view_file
        .write_all(discussion_view_body.as_bytes())
        .with_context(|| format!("Unable to write to file for {:?}", discussion_view_json))?;

    let discussion_view_result = serde_json::from_str::<canvas::DiscussionView>(&discussion_view_body);
    let mut attachments_all = Vec::new();
    match discussion_view_result {
        Result::Ok(discussion_view) => {
            for view in discussion_view.view {
                if let Some(message) = view.message {
                    fork!(
                        process_html_links,
                        (message, path.clone()),
                        (String, PathBuf),
                        options.clone()
                    )
                }
                if let Some(mut attachments) = view.attachments {
                    attachments_all.append(&mut attachments);
                }
                if let Some(attachment) = view.attachment {
                    attachments_all.push(attachment);
                }
            }
        }
        Result::Err(e) => {
            eprintln!("Error when getting submissions at link:{url}, path:{path:?}\n{e:?}",);
        }
    }

    let files = attachments_all
        .into_iter()
        .map(|mut f| {
            f.display_name = format!("{}_{}", f.id, &f.display_name);
            f
        })
        .collect();
    let mut filtered_files = filter_files(&options, &path, files);
    let mut lock = options.files_to_download.lock().await;
    lock.append(&mut filtered_files);

    Ok(())
}

async fn process_files((url, path): (String, PathBuf), options: Arc<ProcessOptions>) -> Result<()> {
    let pages = get_pages(url, &options).await?;

    // For each page
    for pg in pages {
        let uri = pg.url().to_string();
        let files_result = pg.json::<canvas::FileResult>().await;

        match files_result {
            // Got files
            Ok(canvas::FileResult::Ok(files)) => {
                let mut filtered_files = filter_files(&options, &path, files);
                let mut lock = options.files_to_download.lock().await;
                lock.append(&mut filtered_files);
            }

            // Got status code
            Ok(canvas::FileResult::Err { status }) => {
                let course_has_no_files = status == "unauthorized";
                if !course_has_no_files {
                    eprintln!(
                        "Failed to access files at link:{uri}, path:{path:?}, status:{status}",
                    );
                }
            }

            // Parse error
            Err(e) => {
                eprintln!("Error when getting files at link:{uri}, path:{path:?}\n{e:?}",);
            }
        };
    }

    Ok(())
}

fn filter_files(options: &ProcessOptions, path: &Path, files: Vec<File>) -> Vec<File> {
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

    // only download files that do not exist or are updated
    files
        .into_iter()
        .map(|mut f| {
            let sanitized_filename = sanitize_filename::sanitize(&f.display_name);
            f.filepath = path.join(sanitized_filename);
            f
        })
        .filter(|f| !f.locked_for_user)
        .filter(|f| {
            if DateTime::parse_from_rfc3339(&f.updated_at).is_ok() {
                return true;
            }
            eprintln!(
                "Failed to parse updated_at time for {}, {}",
                f.display_name, f.updated_at
            );
            false
        })
        .filter(|f| {
            !f.filepath.exists() || (updated(&f.filepath, &f.updated_at) && options.download_newer)
        })
        .collect()
}

async fn process_html_links(
    (html, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {

    // If file link is part of course files
    let re = Regex::new(r"/courses/[0-9]+/files/[0-9]+").unwrap();
    let file_links = Document::from(html.as_str())
        .find(Name("a"))
        .filter_map(|n| n.attr("href"))
        .filter(|x| x.starts_with(&options.canvas_url))
        .map(|x| Url::parse(x))
        .filter(|x| x.is_ok())
        .map(|x| x.unwrap())
        .filter(|x| re.is_match(x.path()))
        .map(|x| format!("{}/api/v1{}", options.canvas_url, x.path()))
        .collect::<Vec<String>>();
    
    let mut link_files = join_all(file_links.into_iter()
        .map(|x| process_file_id((x, path.clone()), options.clone())))
        .await
        .into_iter()
        .filter_map(|x| x.ok())
        .collect::<Vec<File>>();

    // If image is from canvas it is likely the file url gives permission denied, so download from the CDN
    let image_links = Document::from(html.as_str())
        .find(Name("img"))
        .filter_map(|n| n.attr("src"))
        .filter(|x| x.starts_with(&options.canvas_url))
        .filter(|x| !x.contains("equation_images"))
        .map(|x| x.to_string())
        .collect::<Vec<String>>();
    
    link_files.append(join_all(image_links.into_iter()
        .map(|x| prepare_link_for_download((x, path.clone()), options.clone())))
        .await
        .into_iter()
        .filter_map(|x| x.ok())
        .collect::<Vec<File>>().as_mut());

    let mut filtered_files = filter_files(&options, &path, link_files);
    let mut lock = options.files_to_download.lock().await;
    lock.append(&mut filtered_files);

    Ok(())
}

async fn process_file_id(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<File> {
    let file_resp = get_canvas_api(url.clone(), &options).await?;
    let file_result = file_resp.json::<canvas::File>().await;
    match file_result {
        Result::Ok(mut file) => {
            let file_path = path.join(&file.display_name);
            file.filepath = file_path;
            return Ok(file);
        }
        Err(e) => {
            eprintln!("Error when getting file info at link:{url}, path:{path:?}\n{e:?}",);
            return Err(Into::into(e));
        }
    }
}
async fn prepare_link_for_download(
    (link, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<File> {

    let resp = options
        .client
        .head(&link)
        .bearer_auth(&options.canvas_token)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let headers = resp.headers();
    // get filename out of Content-Disposition header
    let filename = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| {
            let re = Regex::new(r#"filename="(.*)""#).unwrap();
            re.captures(x)
        })
        .and_then(|x| x.get(1))
        .map(|x| x.as_str())
        .unwrap_or_else(|| {
            let re = Regex::new(r"/([^/]+)$").unwrap();
            re.captures(&link)
                .and_then(|x| x.get(1))
                .map(|x| x.as_str())
                .unwrap_or("unknown")
        });
    // last-modified header to TZ string
    let updated_at = headers
        .get(header::LAST_MODIFIED)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| {
            let dt = DateTime::parse_from_rfc2822(x).ok()?;
            Some(dt.with_timezone(&Local).to_rfc3339())
        })
        .unwrap_or_else(|| Local::now().to_rfc3339());
    
    let file = File {
        id: 0,
        folder_id: 0,
        display_name: filename.to_string(),
        size: 0,
        url: link.clone(),
        updated_at: updated_at,
        locked_for_user: false,
        filepath: path.join(filename),
    };
    Ok(file)
}

async fn get_pages(link: String, options: &ProcessOptions) -> Result<Vec<Response>> {
    fn parse_next_page(resp: &Response) -> Option<String> {
        // Parse LINK header
        let links = resp.headers().get(header::LINK)?.to_str().ok()?; // ok to not have LINK header
        let rels = parse_link_header::parse_with_rel(links).unwrap_or_else(|e| {
            panic!(
                "Error parsing header for next page, uri={}, err={e:?}",
                resp.url()
            )
        });

        // Is last page?
        let nex = rels.get("next")?; // ok to not have "next"
        let cur = rels
            .get("current")
            .unwrap_or_else(|| panic!("Could not find current page for {}", resp.url()));
        let last = rels
            .get("last")
            .unwrap_or_else(|| panic!("Could not find last page for {}", resp.url()));
        if cur == last {
            return None;
        };

        // Next page
        Some(nex.raw_uri.clone())
    }

    let mut link = Some(link);
    let mut resps = Vec::new();

    while let Some(uri) = link {
        // GET request
        let resp = get_canvas_api(uri, options).await?;

        // Get next page before returning for json
        link = parse_next_page(&resp);
        resps.push(resp);
    }

    Ok(resps)
}

async fn get_canvas_api(url: String, options: &ProcessOptions) -> Result<Response> {
    let mut query_pairs : Vec<(String, String)> = Vec::new();
    // insert into query_pairs from url.query_pairs();
    for (key, value) in Url::parse(&url)?.query_pairs() {
        query_pairs.push((key.to_string(), value.to_string()));
    }
    for retry in 0..3 {
        let resp = options
            .client
            .get(&url)
            .query(&query_pairs)
            .bearer_auth(&options.canvas_token)
            .timeout(Duration::from_secs(10))
            .send()
            .await;

        match resp {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::FORBIDDEN || retry == 2 {
                    return Ok(resp)
                }
            },
            Err(e) => {println!("Canvas request error uri: {} {}", url, e); return Err(e.into())},
        }

        let wait_time = Duration::from_millis(rand::thread_rng().gen_range(0..1000 * 2_u64.pow(retry)));
        println!("Got 403 for {}, waiting {:?} before retrying, retry {}", url, wait_time, retry);
        tokio::time::sleep(wait_time).await;
        
    }
    Err(Error::msg("canvas request failed"))
}

mod canvas {
    use std::sync::atomic::AtomicUsize;

    use serde::{Deserialize, Serialize};
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
        pub enrollment_term_id: u32,
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

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum PageResult {
        Err { status: String },
        Ok(Vec<Page>),
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct Page {
        pub page_id: u32,
        pub url: String,
        pub title: String,
        pub updated_at: String,
        pub locked_for_user: bool,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct PageBody {
        pub page_id: u32,
        pub url: String,
        pub title: String,
        pub body: String,
        pub updated_at: String,
        pub locked_for_user: bool,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum DiscussionResult {
        Err { status: String },
        Ok(Vec<Discussion>),
    }
    #[derive(Clone, Debug, Deserialize)]
    pub struct Discussion {
        pub id: u32,
        pub title: String,
        pub message: String,
        pub attachments: Vec<File>,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct DiscussionView {
        pub unread_entries: Vec<u32>,
        pub view: Vec<Comments>,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct Comments {
        pub id: u32,
        pub message: Option<String>,
        pub attachment: Option<File>,
        pub attachments: Option<Vec<File>>,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct File {
        pub id: u32,
        pub folder_id: u32,
        pub display_name: String,
        pub size: u64,
        pub url: String,
        pub updated_at: String,
        pub locked_for_user: bool,
        #[serde(skip)]
        pub filepath: std::path::PathBuf,
    }

    pub struct ProcessOptions {
        pub canvas_token: String,
        pub canvas_url: String,
        pub client: reqwest::Client,
        // Process
        pub download_newer: bool,
        pub files_to_download: Mutex<Vec<File>>,
        // Download
        pub progress_bars: indicatif::MultiProgress,
        pub progress_style: indicatif::ProgressStyle,
        // Synchronization
        pub n_active_requests: AtomicUsize, // main() waits for this to be 0
        pub sem_requests: tokio::sync::Semaphore, // Limit #active requests
        pub notify_main: tokio::sync::Notify,
    }
}
