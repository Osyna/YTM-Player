# YTM-Player

YTM-Player is a Bash script that allows you to play audio from YouTube videos directly in your terminal. It provides a simple interface with play/pause functionality and a progress bar.

## Features

- Play audio from YouTube videos without video playback
- Simple terminal-based user interface
- Play/Pause functionality
- Progress bar display
- Displays current track title

## Prerequisites

Before you begin, ensure you have the following dependencies installed:

- yt-dlp
- mpv
- jq
- socat
- bc

## Installation

1. Clone the repository:
   ```
   git clone https://github.com/Osyna/YTM-Player.git
   ```

2. Change to the project directory:
   ```
   cd YTM-Player
   ```

3. Make the script executable:
   ```
   chmod +x ytmplayer.sh
   ```

4. Install dependencies (Ubuntu/Debian example):
   ```
   sudo apt update
   sudo apt install mpv jq socat bc
   sudo curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o /usr/local/bin/yt-dlp
   sudo chmod a+rx /usr/local/bin/yt-dlp
   ```

   Note: Installation commands may vary depending on your operating system. Please refer to the official documentation for each dependency for specific installation instructions.

## Usage

Run the script with a YouTube URL as an argument:

```
./ytmplayer.sh https://www.youtube.com/watch?v=dQw4w9WgXcQ
```

### Controls

- `p`: Toggle Play/Pause
- `q`: Quit the player

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Acknowledgments

- Thanks to the creators of yt-dlp, mpv, jq, and socat

## Support

If you encounter any problems or have any suggestions, please open an issue on the GitHub repository.
