#!/bin/bash
# Cycle through power profiles: power-saver -> balanced -> performance -> power-saver

# Check if powerprofilesctl is available
if ! command -v powerprofilesctl &> /dev/null; then
    exit 0
fi

# Get current profile
current=$(powerprofilesctl get 2>/dev/null)

# Cycle to next profile
case "$current" in
    "power-saver")
        powerprofilesctl set balanced
        ;;
    "balanced")
        powerprofilesctl set performance
        ;;
    "performance")
        powerprofilesctl set power-saver
        ;;
    *)
        # Default to balanced if unknown
        powerprofilesctl set balanced
        ;;
esac
