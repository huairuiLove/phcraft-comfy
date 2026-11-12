// The system document picker, driven from safe Rust through a small C ABI (src/picker.rs).
// The delegate lives here, not in Rust, because implementing an ObjC protocol needs
// objc2's declare_class! (unsafe); everything UIKit stays in this file, force-linked like
// PCSceneDelegate.m. Open picks are read inside the delegate (security scope) and delivered
// as (name, bytes) callbacks; the export sheet copies an already-staged file to wherever
// the user picks. Mode is tracked so an export's didPick is not mistaken for an open.
#import <UIKit/UIKit.h>

typedef void (*pc_open_cb)(const char *name, const uint8_t *bytes, size_t len, int last);
typedef void (*pc_cancel_cb)(void);

@class PCDocumentPickerDelegate;
static UIViewController *PCRootViewController(void);

// Declared early: pc_pick_open/pc_pick_export below reference the shared delegate.
@interface PCDocumentPickerDelegate : NSObject <UIDocumentPickerDelegate>
+ (instancetype)shared;
@end

static pc_open_cb PCOpenCallback;
static pc_cancel_cb PCCancelCallback;
static BOOL PCExportMode;

__attribute__((visibility("default"))) void pc_set_callbacks(pc_open_cb open, pc_cancel_cb cancel) {
    PCOpenCallback = open;
    PCCancelCallback = cancel;
}

__attribute__((visibility("default"))) void pc_pick_open(int allow_multiple) {
    dispatch_async(dispatch_get_main_queue(), ^{
        UIViewController *root = PCRootViewController();
        if (root == nil || PCOpenCallback == NULL) {
            if (PCCancelCallback != NULL) PCCancelCallback();
            return;
        }
        PCExportMode = NO;
        UIDocumentPickerViewController *picker = [[UIDocumentPickerViewController alloc]
            initWithDocumentTypes:@[ @"public.data" ]   // every concrete file type conforms; PSD/PNG/JPEG included
                            inMode:UIDocumentPickerModeOpen];
        picker.delegate = [PCDocumentPickerDelegate shared];
        picker.allowsMultipleSelection = (allow_multiple != 0);
        [root presentViewController:picker animated:YES completion:nil];
    });
}

// Present the export sheet for a file the shell already staged in the app's tmp directory;
// the system copies it to the chosen destination (asCopy leaves our staged file alone).
// The C string is copied into an NSString BEFORE dispatching: a block captures a `const char *`
// by value (the pointer, not the bytes), and the caller's CString is freed when pc_pick_export
// returns — reading it inside the block was a use-after-free that handed the picker a garbage
// path, and initForExportingURLs then aborted the app (it requires an existing file).
__attribute__((visibility("default"))) void pc_pick_export(const char *path) {
    NSString *staged = (path == NULL) ? nil : [NSString stringWithUTF8String:path];
    dispatch_async(dispatch_get_main_queue(), ^{
        UIViewController *root = PCRootViewController();
        if (root == nil || staged == nil) {
            if (PCCancelCallback != NULL) PCCancelCallback();
            return;
        }
        PCExportMode = YES;
        NSURL *url = [NSURL fileURLWithPath:staged];
        // initForExportingURLs raises NSInternalInconsistencyException (aborting the process)
        // unless every URL names an existing file; the shell stages the file first, but a
        // vanished temp file must cancel the export, not kill the app.
        if (![[NSFileManager defaultManager] fileExistsAtPath:url.path]) {
            NSLog(@"PCDP export staged file missing: %@", url.path);
            if (PCCancelCallback != NULL) PCCancelCallback();
            return;
        }
        UIDocumentPickerViewController *picker =
            [[UIDocumentPickerViewController alloc] initForExportingURLs:@[ url ] asCopy:YES];
        picker.delegate = [PCDocumentPickerDelegate shared];
        [root presentViewController:picker animated:YES completion:nil];
    });
}

// winit's classic path keeps no root view controller pointer Rust could reach: find the first
// window of the first connected window scene (the one PCSceneDelegate attached).
static UIViewController *PCRootViewController(void) {
    for (UIScene *scene in UIApplication.sharedApplication.connectedScenes) {
        if (![scene isKindOfClass:[UIWindowScene class]]) continue;
        UIWindowScene *windowScene = (UIWindowScene *)scene;
        if (windowScene.keyWindow.rootViewController != nil) {
            return windowScene.keyWindow.rootViewController;
        }
        for (UIWindow *window in windowScene.windows) {
            if (window.rootViewController != nil) return window.rootViewController;
        }
    }
    return nil;
}

@implementation PCDocumentPickerDelegate

static PCDocumentPickerDelegate *PCSharedDelegate;

+ (instancetype)shared {
    if (PCSharedDelegate == nil) PCSharedDelegate = [[self alloc] init];
    return PCSharedDelegate;
}

- (void)documentPicker:(UIDocumentPickerViewController *)controller
    didPickDocumentsAtURLs:(NSArray<NSURL *> *)urls {
    [controller dismissViewControllerAnimated:YES completion:nil];
    if (PCExportMode) {
        // The system copied the staged file to the picked destination; nothing to read back
        // (the shell's `write_unconfirmed` covers the user backing out instead).
        return;
    }
    NSUInteger last = urls.count - 1;
    for (NSUInteger index = 0; index < urls.count; index++) {
        NSURL *url = urls[index];
        NSString *name = url.lastPathComponent ?: @"file";
        NSData *data = nil;
        BOOL scoped = [url startAccessingSecurityScopedResource];
        data = [NSData dataWithContentsOfURL:url options:0 error:nil];
        if (scoped) [url stopAccessingSecurityScopedResource];
        if (PCOpenCallback == NULL) return;
        if (data != nil) {
            PCOpenCallback(name.UTF8String ?: "file", data.bytes, data.length, (int)(index == last));
        } else if (index == last) {
            PCOpenCallback("", NULL, 0, 1);  // unreadable selection: resolve as empty, not a hang
        }
    }
}

- (void)documentPickerWasCancelled:(UIDocumentPickerViewController *)controller {
    [controller dismissViewControllerAnimated:YES completion:nil];
    if (!PCExportMode && PCCancelCallback != NULL) PCCancelCallback();
}

@end
