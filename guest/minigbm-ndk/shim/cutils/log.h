/* NDK shim for the platform <cutils/log.h> used by minigbm's C core. */
#pragma once
#include <android/log.h>
#ifndef LOG_TAG
#define LOG_TAG "minigbm"
#endif
#define ALOGE(...) __android_log_print(ANDROID_LOG_ERROR, LOG_TAG, __VA_ARGS__)
#define ALOGW(...) __android_log_print(ANDROID_LOG_WARN, LOG_TAG, __VA_ARGS__)
#define ALOGI(...) __android_log_print(ANDROID_LOG_INFO, LOG_TAG, __VA_ARGS__)
#define ALOGD(...) __android_log_print(ANDROID_LOG_DEBUG, LOG_TAG, __VA_ARGS__)
#define ALOGV(...) ((void)0)
