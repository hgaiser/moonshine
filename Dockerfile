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
    sudo curl wget dbus xorg-xwayland

# 3. Install Steam
RUN pacman -S --noconfirm steam && \
    pacman -Scc --noconfirm

# 4. Install Lutris, Wine and dependencies
RUN pacman -S --noconfirm \
    lutris wine-staging winetricks \
    gamemode lib32-gamemode mangohud lib32-mangohud \
    giflib lib32-giflib libpng lib32-libpng libldap lib32-libldap gnutls lib32-gnutls \
    mpg123 lib32-mpg123 openal lib32-openal v4l-utils lib32-v4l-utils libpulse lib32-libpulse \
    libgpg-error lib32-libgpg-error alsa-plugins lib32-alsa-plugins alsa-lib lib32-alsa-lib \
    libjpeg-turbo lib32-libjpeg-turbo sqlite lib32-sqlite libxcomposite lib32-libxcomposite \
    libxinerama lib32-libxinerama ncurses lib32-ncurses opencl-icd-loader lib32-opencl-icd-loader \
    libxslt lib32-libxslt libva lib32-libva gtk3 lib32-gtk3 gst-plugins-base-libs \
    lib32-gst-plugins-base-libs vulkan-icd-loader lib32-vulkan-icd-loader && \
    pacman -Scc --noconfirm

# 5. Create unprivileged user (needed for AUR builds)
RUN useradd -m -s /bin/bash -u 1000 moonshine && \
    usermod -aG video,audio,render,input moonshine && \
    echo "moonshine ALL=(ALL) NOPASSWD: ALL" >> /etc/sudoers

# 6. Install ES-DE & Moonshine via AUR
RUN pacman -S --noconfirm git base-devel && \
    sudo -u moonshine bash -c "git clone https://aur.archlinux.org/yay-bin.git /tmp/yay-bin && cd /tmp/yay-bin && makepkg -si --noconfirm" && \
    sudo -u moonshine bash -c "yay -S --noconfirm emulationstation-de moonshine" && \
    rm -rf /tmp/yay-bin /home/moonshine/.cache/yay && \
    pacman -Scc --noconfirm

# 7. Set up entrypoint
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# NVIDIA runtime variables
ENV NVIDIA_VISIBLE_DEVICES=all
ENV NVIDIA_DRIVER_CAPABILITIES=all

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["moonshine"]
