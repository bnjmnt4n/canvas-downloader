# canvas-downloader

## Description
Downloads files from all courses in canvas.

## Usage
1. Create a credential json file, eg `cred.json`
```json
{
  "canvasUrl": "https://canvas.nus.edu.sg",
  "canvasToken": "12345~jfkdlejoiferjiofu"
}
```
  - `canvasUrl` should include "https://"
  - `canvasToken` can be created from Account > Settings > New Access Token
2. Get Term IDs by running `canvas-downloader` with the credential file, eg
```shell
$ canvas-downloader --credential-file cred.json
Please provide the Term ID(s) to download via -t
Term IDs  | Courses
115       | ["CS1101S", "CS1231S"]
120       | ["CS2040S", "CS2030"]
125       | ["CS3230"]
```
3. Rerun `canvas-downloader` with the terms you are interested in downloading, eg
```shell
$ canvas-downloader --credential-file cred.json -t 115 120
Courses found:
  * CS1101S
  * CS1231S
  ...
```

### Additional Options
- To explore more options, use `--help` or `-h`
```shell
$ canvas-downloader --help
Usage: canvas-downloader [OPTIONS] --credential-file <FILE>

Options:
  -c, --credential-file <FILE>       
  -d, --destination-folder <FOLDER>  [default: .]
  -n, --download-newer               
  -t, --term-ids <ID>...             
  -h, --help                         Print help
  -V, --version                      Print version
```
- If you want to download files updated on canvas, use `--download-newer` or `-n`. By default, files updated on canvas will not overwrite already downloaded files. 
- If you want to specify where to download files into, use `--destination-folder` or `-d`. By default, files will be downloaded to the folder in which the program is called.

### Note for macOS
- To use the executable downloaded from **Releases**, use `xattr` to remove the quarantine
  - e.g. `xattr -d com.apple.quarantine canvas-downloader`
- This occurs because the executable has not been signed with an apple developer account
- If it is not showing up as an executable
  - `chmod +x canvas-downloader`
  - This should make it executable

