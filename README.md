# canvas-downloader

## Description
Downloads files from all courses in canvas.

## Note for macOS
- To use the executable downloaded from **Releases**, use `xattr` to remove the quarantine
    - e.g. `xattr -d com.apple.quarantine canvas-downloader`
- This occurs because the executable has not been signed with an apple developer account
- If it is not showing up as an executable
    - `chmod +x canvas-downloader`
    - This should make it executable

## Usage
- Get your canvas url (i.e. `https://canvas.example.com`)
    - Ensure the url has 'https://'
    - This url is the one that you would use to access the canvas website for your institution
- Get your api token
    - You can create one under Account > Settings > New Access Token
- `./canvas-downloader -u <CANVAS URL> -t <CANVAS API TOKEN> -d <DESTINATION FOLDER>`
    - Manually pass in the url and token to canvas and save the files to the destination folder
    - e.g. command: `./canvas-downloader -u https://canvas.example.com -t 12345~jfkdlejoiferjiofudjsifokjewqifropjdislfjeiwljfpejiopfejsdaojfodd -d ~/courses`
- `./canvas-downloader -u <CANVAS URL> -t <CANVAS API TOKEN> -d <DESTINATION FOLDER> -s -c <CREDENTIAL PATH>`
    - Same as the first command but this saves the url and token as a json file to `<CREDENTIAL PATH>`
    - `-s` tells it to save the credentials to `<CREDENTIAL PATH>`
    - The file will be created if it does not exists
    - e.g. command: `./canvas-downloader -u https://canvas.example.com -t 12345~jfkdlejoiferjiofudjsifokjewqifropjdislfjeiwljfpejiopfejsdaojfodd -d ~/courses -s -c ~/credentials.json`
- `./canvas-downloader -d <DESTINATION FOLDER> -c <CREDENTIAL PATH>`
    - Same as the first command but reads the credentials from `<CREDENTIAL PATH>`
    - e.g. command: `./canvas-downloader -d ~/courses -c ~/credentials.json`
- Recommended to alias the command to use `-u` and `-t`, or `-c` to avoid typing so much
- The downloader will not download the file if there is already a file at where it should be saved to
    - If you want the new version, you need to use the `-n` flag
- By default, the downloader will download all active courses, even from previous terms. Use the `-i` flag with a specific term_id to choose a specific enrollment month (`120` for Jan 2023, for example - this number increments every month).
