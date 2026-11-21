#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>

typedef void (*PickCallback)(void *, const char *, const void *, size_t, const char *);
typedef void (*RestoreCallback)(void *, void *, const char *, const char *);

// AppKit state is used exclusively on the main thread.
static NSOpenPanel *activePanel;
static BOOL stopping;

bool kb_picker_has_gui(void) {
    CFDictionaryRef session = CGSessionCopyCurrentDictionary();
    if (session == NULL) {
        return false;
    }
    CFRelease(session);
    return true;
}

void kb_picker_run(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        [NSApp run];
    }
}

void kb_picker_cancel(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        stopping = YES;
        [activePanel cancel:nil];
    });
}

void kb_picker_stop(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        [NSApp stop:nil];
        // The application loop observes stop after it processes the next event.
        [NSApp postEvent:[NSEvent otherEventWithType:NSEventTypeApplicationDefined
            location:NSZeroPoint modifierFlags:0 timestamp:0 windowNumber:0
            context:nil subtype:0 data1:0 data2:0] atStart:YES];
    });
}

void kb_picker_select(const char *initial, void *context, PickCallback callback) {
    NSString *location = initial == NULL ? nil : [NSString stringWithUTF8String:initial];
    dispatch_async(dispatch_get_main_queue(), ^{
        if (stopping || activePanel != nil) {
            callback(context, NULL, NULL, 0, "文件夹选择窗口已打开，或服务正在关闭。");
            return;
        }
        NSOpenPanel *panel = [NSOpenPanel openPanel];
        activePanel = panel;
        panel.canChooseFiles = NO;
        panel.canChooseDirectories = YES;
        panel.allowsMultipleSelection = NO;
        panel.canCreateDirectories = NO;
        panel.title = @"选择监控文件夹";
        panel.prompt = @"监控此文件夹";
        panel.message = @"本地知识库会读取此文件夹及其子文件夹中的受支持文件。";
        if (location != nil) {
            panel.directoryURL = [NSURL fileURLWithPath:location.stringByExpandingTildeInPath isDirectory:YES];
        }
        if (@available(macOS 14.0, *)) {
            [NSApp activate];
        } else {
            [NSApp activateIgnoringOtherApps:YES];
        }
        [panel beginWithCompletionHandler:^(NSModalResponse response) {
            activePanel = nil;
            if (response != NSModalResponseOK || panel.URL == nil || stopping) {
                callback(context, NULL, NULL, 0, NULL);
                return;
            }
            NSURL *url = panel.URL;
            NSError *error = nil;
            NSData *bookmark = [url bookmarkDataWithOptions:NSURLBookmarkCreationWithSecurityScope |
                NSURLBookmarkCreationSecurityScopeAllowOnlyReadAccess
                includingResourceValuesForKeys:nil relativeToURL:nil error:&error];
            [url stopAccessingSecurityScopedResource];
            if (bookmark == nil) {
                callback(context, NULL, NULL, 0, error.localizedDescription.UTF8String ?: "无法保存文件夹授权。");
                return;
            }
            callback(context, url.fileSystemRepresentation, bookmark.bytes, bookmark.length, NULL);
        }];
        [panel orderFrontRegardless];
        // Release the pending request if the picker is left unattended.
        __weak NSOpenPanel *pendingPanel = panel;
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 180 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
            if (activePanel != nil && activePanel == pendingPanel) {
                [activePanel cancel:nil];
            }
        });
    });
}

void kb_picker_restore(const void *bytes, size_t size, void *context, RestoreCallback callback) {
    @autoreleasepool {
        NSData *bookmark = [NSData dataWithBytes:bytes length:size];
        NSError *error = nil;
        BOOL stale = NO;
        NSURL *url = [NSURL URLByResolvingBookmarkData:bookmark
            options:NSURLBookmarkResolutionWithSecurityScope | NSURLBookmarkResolutionWithoutUI |
                NSURLBookmarkResolutionWithoutMounting
            relativeToURL:nil bookmarkDataIsStale:&stale error:&error];
        if (url == nil || !url.isFileURL || ![url startAccessingSecurityScopedResource]) {
            callback(context, NULL, NULL, error.localizedDescription.UTF8String ?: "macOS 未授予文件夹访问权限。");
            return;
        }
        callback(context, (__bridge_retained void *)url, url.fileSystemRepresentation, NULL);
    }
}

void kb_picker_release(void *handle) {
    @autoreleasepool {
        NSURL *url = (__bridge_transfer NSURL *)handle;
        [url stopAccessingSecurityScopedResource];
    }
}
