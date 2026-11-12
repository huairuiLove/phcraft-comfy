// The system document picker and the photo album picker (PHPicker), driven from safe Rust
// through a small C ABI (src/picker.rs). The delegates live here, not in Rust, because
// implementing an ObjC protocol needs objc2's declare_class! (unsafe); everything UIKit stays
// in this file, force-linked like PCSceneDelegate.m. Open picks are read inside the delegate
// (security scope) and delivered as (name, bytes) callbacks; the export sheet copies an
// already-staged file to wherever the user picks. Mode is tracked so an export's didPick is
// not mistaken for an open. The album picker (pc_pick_photos) delivers through the same
// callback: each picked photo's encoded bytes (HEIC/JPEG/PNG, as stored), then the end marker.
#import <UIKit/UIKit.h>
#import <PhotosUI/PhotosUI.h>

typedef void (*pc_open_cb)(const char *name, const uint8_t *bytes, size_t len, int last);
typedef void (*pc_cancel_cb)(void);

@class PCDocumentPickerDelegate;
static UIViewController *PCRootViewController(void);

// Declared early: pc_pick_open/pc_pick_export/pc_pick_photos below reference the shared delegate.
@interface PCDocumentPickerDelegate : NSObject <UIDocumentPickerDelegate, PHPickerViewControllerDelegate>
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

// The photo album picker (PHPicker): out-of-process, so no photo-library permission is needed.
// Images only; selectionLimit 0 means "as many as the user picks". Results arrive as item
// providers whose image representation is loaded asynchronously and delivered through the open
// callback (one per photo, then the end marker); backing out with nothing picked cancels.
__attribute__((visibility("default"))) void pc_pick_photos(int allow_multiple) {
    dispatch_async(dispatch_get_main_queue(), ^{
        UIViewController *root = PCRootViewController();
        if (root == nil || PCOpenCallback == NULL) {
            if (PCCancelCallback != NULL) PCCancelCallback();
            return;
        }
        PCExportMode = NO;
        PHPickerConfiguration *configuration = [[PHPickerConfiguration alloc] init];
        configuration.filter = [PHPickerFilter imagesFilter];
        configuration.selectionLimit = (allow_multiple != 0) ? 0 : 1;
        PHPickerViewController *picker = [[PHPickerViewController alloc] initWithConfiguration:configuration];
        picker.delegate = [PCDocumentPickerDelegate shared];
        [root presentViewController:picker animated:YES completion:nil];
    });
}

// Present the export sheet for a file the shell already staged in the app's tmp directory;
// the system copies it to the chosen destination (asCopy leaves our staged file alone).
__attribute__((visibility("default"))) void pc_pick_export(const char *path) {
    dispatch_async(dispatch_get_main_queue(), ^{
        UIViewController *root = PCRootViewController();
        if (root == nil || path == NULL) {
            if (PCCancelCallback != NULL) PCCancelCallback();
            return;
        }
        PCExportMode = YES;
        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:path]];
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

// The album picker's answer. Each result's image representation loads asynchronously; the
// open callback fires per loaded photo and once more (empty, last=1) when all loads settled,
// so the Rust side resolves the batch as it does a multi-file open.
- (void)picker:(PHPickerViewController *)picker didFinishPickingWithResults:(NSArray<PHPickerResult *> *)results {
    [picker dismissViewControllerAnimated:YES completion:nil];
    if (results.count == 0) {
        if (PCCancelCallback != NULL) PCCancelCallback();
        return;
    }
    __block NSUInteger remaining = results.count;
    void (^settled)(void) = ^{
        if (--remaining == 0 && PCOpenCallback != NULL) PCOpenCallback("", NULL, 0, 1);
    };
    for (PHPickerResult *result in results) {
        NSItemProvider *provider = result.itemProvider;
        NSString *name = provider.suggestedName ?: @"photo";
        if (![provider hasItemConformingToTypeIdentifier:@"public.image"]) {
            settled();
            continue;
        }
        [provider loadDataRepresentationForTypeIdentifier:@"public.image"
            completionHandler:^(NSData * _Nullable data, NSError * _Nullable error) {
                dispatch_async(dispatch_get_main_queue(), ^{
                    if (data != nil && PCOpenCallback != NULL) {
                        PCOpenCallback(name.UTF8String ?: "photo", data.bytes, data.length, 0);
                    }
                    settled();
                });
            }];
    }
}

@end
