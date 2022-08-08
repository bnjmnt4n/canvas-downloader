# Canvas-cli

## Description
Downloads files from all courses in canvas.

## Usage
- `./canvas-downloader -u <CANVAS URL> -t <CANVAS API TOKEN> -d <DESTINATION FOLDER>`
    - manually pass in the url and token to canvas and save the files to the destination folder
- `./canvas-downloader -u <CANVAS URL> -t <CANVAS API TOKEN> -d <DESTINATION FOLDER> -s -c <CREDENTIAL PATH>`
    - same as the first command but this saves the url and token as `<CREDENTIAL PATH>`
    - `-s` tells it to save the credentials to `<CREDENTIAL PATH>`
- `./canvas-downloader -d <DESTINATION FOLDER> -c <CREDENTIAL PATH>`
    - same as the first command but reads the credentials from `<CREDENTIAL PATH>`
- Recommended to alias the command to use `-u` and `-t`, or `-c` to avoid typing so much
