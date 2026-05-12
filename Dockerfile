FROM archlinux:base

# 1. Configure pacman
RUN echo -e "\n[multilib]\nInclude = /etc/pacman.d/mirrorlist" >> /etc/pacman.conf && \
    sed -i 's/^#ParallelDownloads/ParallelDownloads/' /etc/pacman.conf

# 2. Update and install base dependencies, GPU drivers, and audio
RUN pacman -Syu --noconfirm \
    mesa lib32-mesa \
    vulkan-radeon lib32-vulkan-radeon \
    vulkan-intel lib32-vulkan-intel intel-media-driver \
    nvidia-utils lib32-nvidia-utils \
    pipewire pipewire-pulse pipewire-alsa pipewire-jack \
    ttf-liberation ttf-dejavu noto-fonts noto-fonts-cjk \
    sudo curl wget xorg-xwayland

# 3. Install Steam, Lutris, Wine and dependencies
RUN pacman -S --noconfirm \
    steam lutris wine-staging winetricks \
    gamemode lib32-gamemode mangohud lib32-mangohud \
    lib32-giflib lib32-mpg123 openal lib32-openal \
    v4l-utils lib32-v4l-utils lib32-libpulse lib32-libjpeg-turbo \
    lib32-libxcomposite opencl-icd-loader lib32-opencl-icd-loader \
    libxslt lib32-libxslt lib32-gtk3 && \
    pacman -Scc --noconfirm

# 4. Create unprivileged user (needed for AUR builds)
RUN useradd -m -s /bin/bash -u 1000 moonshine && \
    usermod -aG video,audio,render,input moonshine && \
    echo "moonshine ALL=(ALL) NOPASSWD: ALL" >> /etc/sudoers

# 5. Install ES-DE & Moonshine via AUR
RUN pacman -S --noconfirm git base-devel && \
    sudo -u moonshine bash -c "git clone https://aur.archlinux.org/yay-bin.git /tmp/yay-bin && cd /tmp/yay-bin && makepkg -si --noconfirm" && \
    sudo -u moonshine bash -c "yay -S --noconfirm emulationstation-de moonshine" && \
    rm -rf /tmp/yay-bin /home/moonshine/.cache/yay && \
    pacman -Scc --noconfirm

# 6. Set up entrypoint
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# NVIDIA runtime variables
ENV NVIDIA_VISIBLE_DEVICES=all
ENV NVIDIA_DRIVER_CAPABILITIES=all

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["moonshine", "/home/moonshine/.config/moonshine/config.toml"]
