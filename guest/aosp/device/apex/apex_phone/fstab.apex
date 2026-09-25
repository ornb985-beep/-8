# Apex-AOSP: partitions come from the VMM's composite GPT disk
# (/dev/block/by-name/* via androidboot.boot_devices=a000000.virtio_mmio).
#<src>                       <mnt_point>   <type> <mnt_flags>                                  <fs_mgr_flags>
system                       /system       erofs  ro                                           wait,logical,first_stage_mount,avb=vbmeta
system_ext                   /system_ext   erofs  ro                                           wait,logical,first_stage_mount
product                      /product      erofs  ro                                           wait,logical,first_stage_mount
vendor                       /vendor       erofs  ro                                           wait,logical,first_stage_mount
vendor_dlkm                  /vendor_dlkm  erofs  ro                                           wait,logical,first_stage_mount
system_dlkm                  /system_dlkm  erofs  ro                                           wait,logical,first_stage_mount
odm                          /odm          erofs  ro                                           wait,logical,first_stage_mount
/dev/block/by-name/metadata  /metadata     ext4   noatime,nosuid,nodev,sync                    wait,formattable,first_stage_mount,check
/dev/block/by-name/userdata  /data         f2fs   noatime,nosuid,nodev,inlinecrypt,reserve_root=32768 latemount,wait,check,quota,formattable,reservedsize=128M,fileencryption=aes-256-xts:aes-256-cts:v2+inlinecrypt_optimized,keydirectory=/metadata/vold/metadata_encryption,checkpoint=fs
/dev/block/by-name/misc      /misc         emmc   defaults                                     defaults
