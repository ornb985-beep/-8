# Apex fstab (Android 12, legacy system-as-root: the kernel mounts system
# from root=/dev/vda2; first-stage init mounts vendor from the device tree).
/dev/block/by-name/system   /system   ext4   ro                       wait,first_stage_mount
/dev/block/by-name/vendor   /vendor   ext4   ro                       wait,first_stage_mount
/dev/block/by-name/userdata /data     ext4   noatime,nosuid,nodev     wait,formattable
