#
# apex_phone: AOSP product for the Apex-AOSP VMM (Apple Silicon, 120 Hz).
#
# Graphics stack (guest renderer, default):
#   SurfaceFlinger -> HWC3 (drm_hwcomposer) -> virtio-gpu KMS (EDID 120 Hz)
#   GLES via ANGLE on SwiftShader Vulkan, buffers from minigbm (virtgpu).
# Host renderer (profile display.renderer = "gfxstream"):
#   gfxstream guest GLES/Vulkan + ranchu HWC -> host Metal via gfxstream.
#

$(call inherit-product, $(SRC_TARGET_DIR)/product/core_64_bit_only.mk)
$(call inherit-product, $(SRC_TARGET_DIR)/product/generic_system.mk)
$(call inherit-product, $(SRC_TARGET_DIR)/product/handheld_system_ext.mk)
$(call inherit-product, $(SRC_TARGET_DIR)/product/aosp_product.mk)

PRODUCT_NAME := apex_phone
PRODUCT_DEVICE := apex_phone
PRODUCT_BRAND := Apex
PRODUCT_MODEL := Apex One
PRODUCT_MANUFACTURER := Apex

PRODUCT_SHIPPING_API_LEVEL := 36
PRODUCT_CHARACTERISTICS := nosdcard
PRODUCT_USE_DYNAMIC_PARTITIONS := true
PRODUCT_BUILD_SUPER_PARTITION := true
PRODUCT_BUILD_INIT_BOOT_IMAGE := true
PRODUCT_BUILD_VENDOR_BOOT_IMAGE := true
PRODUCT_ENFORCE_VINTF_MANIFEST := true

DEVICE_PATH := device/apex/apex_phone

# --- Graphics -----------------------------------------------------------------
PRODUCT_PACKAGES += \
    android.hardware.composer.hwc3-service.drm \
    android.hardware.graphics.allocator-service.minigbm \
    mapper.minigbm \
    libEGL_angle \
    libGLESv1_CM_angle \
    libGLESv2_angle \
    vulkan.pastel

# ro.hardware.{egl,vulkan,gralloc} are set in init.apex.rc from the VMM's
# bootconfig so one build serves both the guest and the gfxstream renderer.
PRODUCT_VENDOR_PROPERTIES += \
    ro.opengles.version=196610 \
    ro.surface_flinger.max_frame_buffer_acquired_buffers=3 \
    ro.surface_flinger.use_content_detection_for_refresh_rate=false \
    ro.surface_flinger.enable_frame_rate_override=false \
    ro.surface_flinger.has_wide_color_display=false \
    ro.surface_flinger.present_time_offset_from_vsync_ns=0 \
    debug.sf.enable_gl_backpressure=1 \
    debug.hwui.renderer=skiavk \
    ro.hwui.use_vulkan=true

# 120 Hz defaults for the DisplayModeDirector.
PRODUCT_PACKAGES += ApexFrameworkOverlay

# --- Core HALs -------------------------------------------------------------------
PRODUCT_PACKAGES += \
    android.hardware.health-service.example \
    android.hardware.security.keymint-service \
    android.hardware.gatekeeper-service.software \
    android.hardware.boot-service.default \
    android.hardware.power-service.example \
    android.hardware.vibrator-service.example \
    android.hardware.sensors-service.example \
    android.hardware.audio.service \
    android.hardware.audio@7.1-impl \
    android.hardware.audio.effect.service-aidl.example

# --- Boot / storage --------------------------------------------------------------
PRODUCT_COPY_FILES += \
    $(DEVICE_PATH)/fstab.apex:$(TARGET_COPY_OUT_VENDOR_RAMDISK)/first_stage_ramdisk/fstab.apex \
    $(DEVICE_PATH)/fstab.apex:$(TARGET_COPY_OUT_VENDOR)/etc/fstab.apex \
    $(DEVICE_PATH)/init.apex.rc:$(TARGET_COPY_OUT_VENDOR)/etc/init/hw/init.apex.rc \
    $(DEVICE_PATH)/ueventd.apex.rc:$(TARGET_COPY_OUT_VENDOR)/etc/ueventd.rc

# --- Input: virtio-input touchscreen behaves like a phone panel ------------------
PRODUCT_COPY_FILES += \
    $(DEVICE_PATH)/input/Vendor_1d6b_Product_a001.idc:$(TARGET_COPY_OUT_VENDOR)/usr/idc/Vendor_1d6b_Product_a001.idc \
    $(DEVICE_PATH)/input/Vendor_1d6b_Product_a002.kl:$(TARGET_COPY_OUT_VENDOR)/usr/keylayout/Vendor_1d6b_Product_a002.kl \
    frameworks/native/data/etc/android.hardware.touchscreen.multitouch.jazzhand.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.touchscreen.multitouch.jazzhand.xml \
    frameworks/native/data/etc/android.hardware.vulkan.level-1.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.vulkan.level.xml \
    frameworks/native/data/etc/android.hardware.vulkan.version-1_3.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.vulkan.version.xml \
    frameworks/native/data/etc/android.software.opengles.deqp.level-2024-03-01.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.software.opengles.deqp.level.xml

# Density and identity come from the VMM's bootconfig (see init.apex.rc).
PRODUCT_SYSTEM_DEFAULT_PROPERTIES += \
    ro.adb.secure=0
