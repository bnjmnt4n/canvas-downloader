use anyhow::{Context, Result};
use canvas::ProcessOptions;
use clap::Parser;
use futures::{future::BoxFuture, FutureExt};
use std::{sync::Arc, path::PathBuf};
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
            let file = std::fs::File::open(path)?;
            serde_json::from_reader(file).expect("Crendential file is not valid json")
        }
    } else {
        Option::None
    };

    let canvas_url = if credentials.is_some() {
        credentials.clone().unwrap().canvas_url
    } else {
        args.canvas_url.unwrap()
    };

    let canvas_token = if credentials.is_some() {
        credentials.clone().unwrap().canvas_token
    } else {
        args.canvas_token.unwrap()
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

    let courses = client.get(&courses_link)
        .bearer_auth(&canvas_token)
        .send()
        .await
        .with_context(|| format!("Something went wrong when reaching {}", &courses_link))?
        .json::<Vec<canvas::Course>>()
        .await?;

    let options = ProcessOptions {
        canvas_token: canvas_token.clone(),
        link: String::from(""),
        parent_folder_path: PathBuf::new(),
        client: client.clone(),
        files_to_download: Arc::new(Mutex::new(Vec::new())),
    };

    println!("Courses found:");
    for course in courses {
        println!("  * {} - {}", course.course_code, course.name);

        let course_folder_path = args.destination_folder.join(course.course_code);
        if !course_folder_path.exists() {
            std::fs::create_dir(&course_folder_path)
                .with_context(|| format!("Failed to create directory: {}", course_folder_path.to_string_lossy()))?;
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

    println!("Downloading {} file{}", files_to_download.len(), if files_to_download.len() == 1 { "" } else { "s" } );

    let mut join_handles = Vec::new();
    let mut start = 0;
    for i in 0..num_worker_threads {
        let mut work = min_work;
        if i < num_worker_extra_work {
            work += 1;
        }
        let work_start = start;
        let work_end = work_start + work;
        start = work_end;
        let files_to_download = files_to_download.clone();
        let canvas_token = canvas_token.clone();
        let client = client.clone();
        let handle = tokio::spawn(async move {
            for i in work_start..work_end {
                let file = files_to_download.get(i).unwrap();
                println!("Downloading {} to {}", file.filename, file.filepath.to_string_lossy());

                let file_bytes = client.get(&file.url)
                    .bearer_auth(&canvas_token)
                    .send()
                    .await
                    .with_context(|| format!("Something went wrong when reaching {}", &file.url)).unwrap()
                    .bytes()
                    .await.unwrap();

                {
                    let mut file = std::fs::File::create(&file.filepath).unwrap();
                    let mut cursor = std::io::Cursor::new(file_bytes);
                    std::io::copy(&mut cursor, &mut file).unwrap();
                }
            }
        });
        
        join_handles.push(handle);
    }

    for handle in join_handles {
        handle.await?;
    }

    Ok(())
}

// async recursion needs boxing
fn process_folders(options: ProcessOptions) -> BoxFuture<'static, ()> {
    async move {
        let canvas_token = &options.canvas_token;
        let folders = options.client.get(&options.link)
            .bearer_auth(&canvas_token)
            .send()
            .await
            .with_context(|| format!("Something went wrong when reaching {}", &options.link)).unwrap()
            .json::<Vec<canvas::Folder>>()
            .await.unwrap();

        for folder in folders {
            // println!("  * {} - {}", folder.id, folder.name);
            let sanitized_folder_name = sanitize_filename::sanitize(folder.name);
            let folder_path = options.parent_folder_path.join(sanitized_folder_name);
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
    }.boxed()
}

async fn process_files(options: ProcessOptions) {
    let mut files = options.client.get(&options.link)
        .bearer_auth(&options.canvas_token)
        .send()
        .await
        .with_context(|| format!("Something went wrong when reaching {}", &options.link)).unwrap()
        .json::<Vec<canvas::File>>()
        .await.unwrap();

    for file in &mut files {
        let sanitized_filename = sanitize_filename::sanitize(&file.filename);
        file.filepath = options.parent_folder_path.join(sanitized_filename);
    }

    // only download files that do not exist and match their parent folder id
    let mut filtered_files = files.into_iter()
        .filter(|f| !f.filepath.exists())
        .collect::<Vec<canvas::File>>();

    let mut lock = options.files_to_download.lock().await;
    lock.append(&mut filtered_files);
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
    pub struct Folder {
        pub id: u32,
        pub name: String,
        pub folders_url: String,
        pub files_url: String,
        pub for_submissions: bool,
        pub can_upload: bool,
    }
    
    #[derive(Clone, Debug, Deserialize)]
    pub struct File {
        pub id: u32,
        pub folder_id: u32,
        pub filename: String,
        pub size: u64,
        pub url: String,
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
    }
}
