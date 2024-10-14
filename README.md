# YTM-Player

![Alt text](screenshot.png?raw=true "YTM Player Screenshot")

[alt text](https://github.com/Osyna/YTM-Player/blob/[main]/screenshot.png?raw=true)

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

5. Make the script accessible as a global command:
   ```
   sudo ln -s "$(pwd)/ytmplayer.sh" /usr/local/bin/ymp
   ```

   This creates a symbolic link named `ymp` in `/usr/local/bin`, which is typically in your PATH. You may need to restart your terminal or source your shell configuration file for the changes to take effect.

## Usage

You can now run the script from anywhere using the `ymp` command followed by a YouTube URL:

```
ymp https://www.youtube.com/watch?v=dQw4w9WgXcQ
```

### Controls

- `p`: Toggle Play/Pause
- `q`: Quit the player

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Acknowledgments

- Thanks to the creators of yt-dlp, mpv, jq, and socat for their fantastic jobs.

## Support

If you encounter any problems or have any suggestions, please open an issue on the GitHub repository.

## Uninstallation

If you want to remove the global command, you can do so by running:

```
sudo rm /usr/local/bin/ymp
```

To completely uninstall YTM-Player, also remove the cloned repository.
