#!/bin/bash

# YTMPlayer: A Bash Script for Playing YouTube Audio
# Version: 1.0
# Author: Irvin aka Osyna
# 
# Dependencies: yt-dlp, mpv, jq, socat, bc


# Global variables
URL=""
SOCKET="/tmp/mpvsocket_$$"
MPV_PID=""

# Function to display usage information
display_usage() {
    echo "Usage: $0 <YouTube URL>"
    echo "Example: $0 https://www.youtube.com/watch?v=dQw4w9WgXcQ"
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
    local width=30
    local position=$1
    local duration=$2
    if [[ $duration == "0" || $duration == "null" ]]; then
        printf "[%-${width}s]" ""
    else
        local filled=$(echo "scale=0; $width * $position / $duration" | bc 2>/dev/null)
        if [ -z "$filled" ]; then filled=0; fi
        local empty=$((width - filled))
        printf "[%s%s]" "$(printf '%0.s#' $(seq 1 $filled))" "$(printf '%0.s-' $(seq 1 $empty))"
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

# Main function to run the player
run_player() {
    # Start MPV
    mpv --input-ipc-server="$SOCKET" --no-video --no-terminal "$URL" &>/dev/null &
    MPV_PID=$!
    sleep 1  # Give MPV a moment to start

    # Hide cursor
    tput civis

    # Clear the screen once
    clear

    # Main loop
    while true; do
        # Get current state
        read position duration <<< $(get_progress)
        title=$(get_title)
        paused=$(get_property "pause")
        
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
\033[1;36m[p]\033[0m Play/Pause \033[1;36m[q]\033[0m Quit
EOF
)
        # Update the interface without clearing the screen
        tput cup 0 0  # Move cursor to top-left corner
        echo -en "$interface"
        
        # Move cursor to a safe position
        tput cup $(($(tput lines)-1)) 0
        
        # Read user input (with timeout to update progress)
        read -t 0.1 -n 1 cmd
        
        case $cmd in
            p)
                toggle_pause
                ;;
            q) 
                cleanup 
                ;;
        esac
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

    # Set URL
    URL="$1"

    # Set up trap to handle script termination
    trap cleanup EXIT INT TERM

    # Run the player
    run_player
}

# Call main function with all script arguments
main "$@"
