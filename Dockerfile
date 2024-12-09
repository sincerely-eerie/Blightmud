# Use the official Arch Linux image as the base
FROM archlinux:latest

# Update the package lists
RUN pacman -Syu --noconfirm

# Install necessary packages
RUN pacman -S --noconfirm \
    base-devel \
    rust \
    cargo \
    makepkg

# Set the working directory
WORKDIR /src

