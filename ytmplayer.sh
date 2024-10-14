#!/bin/bash

# YTMPlayer: A Bash Script for Playing YouTube Audio
# Version: 2.0
# Author: Irvin aka Osyna
#
# Dependencies: yt-dlp, mpv, jq, socat, bc

# Global variables
URL=""
SOCKET="/tmp/mpvsocket_$$"
MPV_PID=""
IS_PLAYLIST=false
IS_LOADING=false
NEXT_TITLE=""

# Function to display usage information
display_usage() {
    echo "Usage: $0 <YouTube URL or Playlist URL>"
    echo "Example for single video: $0 https://www.youtube.com/watch?v=dQw4w9WgXcQ"
    echo "Example for playlist: $0 https://www.youtube.com/watch?v=XnG3YWYMY-I&list=RDQMxUfpwjvstDY&start_radio=1"
}

# Function to check if all required dependencies are installed
check_dependencies() {
    local deps=("yt-dlp" "mpv" "jq" "socat" "bc")
    for dep in "${deps[@]}"; do
        if ! command -v "$dep" &> /dev/null; then
            echo "Error: $dep is not installed. Please install it and try again."
            exit 1
        fi
    done
}

# Function to send command to MPV
# $1: The command to send
send_command() {
    echo "$1" | socat - "$SOCKET" 2>/dev/null
}

# Function to get property from MPV
# $1: The property to get
get_property() {
    send_command '{ "command": ["get_property", "'"$1"'"] }' | jq -r '.data'
}

# Function to get current playback time and duration
get_progress() {
    local position=$(get_property "time-pos")
    local duration=$(get_property "duration")
    if [[ $position == "null" || $duration == "null" ]]; then
        echo "0 0"
    else
        printf "%.2f %.2f" "$position" "$duration"
    fi
}

# Function to draw progress bar
# $1: Current position
# $2: Total duration
draw_progress_bar() {
    local width=50
    local position=$1
    local duration=$2
    if [[ $duration == "0" || $duration == "null" ]]; then
        printf "[%-${width}s]" ""
    else
        local filled=$(echo "scale=0; $width * $position / $duration" | bc 2>/dev/null)
        if [ -z "$filled" ]; then filled=0; fi
        local empty=$((width - filled))
        printf "[%s%s]" "$(printf '%0.s█' $(seq 1 $filled))" "$(printf '%0.s░' $(seq 1 $empty))"
    fi
}

# Function to get track title
get_title() {
    get_property "media-title"
}

# Function to format time in MM:SS format
# $1: Time in seconds
format_time() {
    local seconds=$(printf "%.0f" "$1")
    printf "%02d:%02d" $((seconds/60)) $((seconds%60))
}

# Function to clean up resources and exit
cleanup() {
    kill $MPV_PID 2>/dev/null
    rm -f "$SOCKET"
    tput cnorm  # Show cursor
    clear
    exit 0
}

# Function to toggle play/pause
toggle_pause() {
    send_command '{ "command": ["cycle", "pause"] }' >/dev/null
}

# Function to seek forward or backward
# $1: Number of seconds to seek (positive for forward, negative for backward)
seek() {
    send_command '{ "command": ["seek", '"$1"', "relative"] }' >/dev/null
}

# Function to play next track in playlist
play_next() {
    send_command '{ "command": ["playlist-next", "force"] }' >/dev/null
}

# Function to play previous track in playlist
play_previous() {
    send_command '{ "command": ["playlist-prev", "force"] }' >/dev/null
}

# Function to get next track title
get_next_title() {
    local next_pos=$(($(get_property "playlist-pos") + 1))
    NEXT_TITLE=$(send_command '{ "command": ["get_property", "playlist/'$next_pos'/title"] }' | jq -r '.data')
}

# Function to clear the screen and reset cursor position
clear_screen() {
    tput clear
    tput cup 0 0
}

# Main function to run the player
run_player() {
    # Start MPV
    if $IS_PLAYLIST; then
        yt-dlp -j --flat-playlist "$URL" | jq -r '.id' | sed 's_^_https://youtu.be/_' > /tmp/playlist.txt
        mpv --input-ipc-server="$SOCKET" --no-video --no-terminal --playlist=/tmp/playlist.txt &>/dev/null &
    else
        mpv --input-ipc-server="$SOCKET" --no-video --no-terminal "$URL" &>/dev/null &
    fi
    MPV_PID=$!
    sleep 1  # Give MPV a moment to start

    # Hide cursor
    tput civis

    # Clear the screen once
    clear_screen

    # Main loop
    while true; do
        # Get current state
        read position duration <<< $(get_progress)
        title=$(get_title)
        paused=$(get_property "pause")
        playlist_pos=$(get_property "playlist-pos")
        playlist_count=$(get_property "playlist-count")

        # Truncate title if too long
        if [ ${#title} -gt 40 ]; then
            title="${title:0:37}..."
        fi

        # Prepare the interface
        interface=$(cat << EOF
\033[1;34m▶ YTMPlayer\033[0m
\033[1;33m$title\033[0m
$(draw_progress_bar $position $duration) $(format_time $position) / $(format_time $duration)
Status: $([ "$paused" = "true" ] && echo "\033[1;31m⏸ PAUSED \033[0m" || echo "\033[1;32m▶ PLAYING\033[0m")
EOF
)
        if $IS_PLAYLIST; then
            interface+=$(cat << EOF

Playlist: $((playlist_pos + 1))/$playlist_count
EOF
)
            if $IS_LOADING; then
                interface+=$(cat << EOF
\033[1;35mLoading next track: $NEXT_TITLE\033[0m
EOF
)
            fi
            interface+=$(cat << EOF

\033[1;36m[p]\033[0m Play/Pause \033[1;36m[←]\033[0m Rewind 5s \033[1;36m[→]\033[0m Forward 5s
\033[1;36m[n]\033[0m Next Track \033[1;36m[b]\033[0m Previous Track \033[1;36m[q]\033[0m Quit
EOF
)
        else
            interface+=$(cat << EOF

\033[1;36m[p]\033[0m Play/Pause \033[1;36m[←]\033[0m Rewind 5s \033[1;36m[→]\033[0m Forward 5s \033[1;36m[q]\033[0m Quit
EOF
)
        fi

        # Clear screen and update the interface
        clear_screen
        echo -en "$interface"

        # Move cursor to a safe position
        tput cup $(($(tput lines)-1)) 0

        # Read user input (with timeout to update progress)
        read -t 0.1 -n 1 cmd

        if [ "$cmd" = $'\e' ]; then
            read -t 0.1 -n 2 arrow
            case "$arrow" in
                "[C") seek 5 ;;    # Right arrow
                "[D") seek -5 ;;   # Left arrow
            esac
        else
            case $cmd in
                p) toggle_pause ;;
                n)
                    if $IS_PLAYLIST; then
                        IS_LOADING=true
                        get_next_title
                        play_next
                        sleep 1
                        IS_LOADING=false
                    fi
                    ;;
                b)
                    if $IS_PLAYLIST; then
                        IS_LOADING=true
                        play_previous
                        sleep 1
                        IS_LOADING=false
                    fi
                    ;;
                q) cleanup ;;
            esac
        fi
    done
}

# Main execution starts here
main() {
    # Check if URL is provided
    if [ $# -eq 0 ]; then
        display_usage
        exit 1
    fi

    # Check dependencies
    check_dependencies

    # Set URL and check if it's a playlist
    URL="$1"
    if [[ "$URL" == *"list="* ]]; then
        IS_PLAYLIST=true
    fi

    # Set up trap to handle script termination
    trap cleanup EXIT INT TERM

    # Run the player
    run_player
}

# Call main function with all script arguments
main "$@"
