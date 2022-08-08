# Canvas-cli

## Description
Downloads files from all courses in canvas.

## Usage
- `./cavnas-downloader -u <CANVAS URL> -t <CANVAS API TOKEN> -d <DESTINATION FOLDER>`
    - manually pass in the url and token to canvas and saves the files to the destination folder
- `./cavnas-downloader -u <CANVAS URL> -t <CANVAS API TOKEN> -d <DESTINATION FOLDER> -s -c <CREDENTIAL PATH>`
    - same as the first command but this saves the url and token as a json file to `<CREDENTIAL PATH>`
    - `-s` tells it to save the credentials to `<CREDENTIAL PATH>`
- `./cavnas-downloader -d <DESTINATION FOLDER> -c <CREDENTIAL PATH>`
    - same as the first command but reads the credentials from `<CREDENTIAL PATH>`
- Recommended to alias the command to use `-u` and `-t`, or `-c` to avoid typing so much
