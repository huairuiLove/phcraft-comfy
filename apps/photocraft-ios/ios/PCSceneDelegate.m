#import <UIKit/UIKit.h>
#import <objc/runtime.h>

static UIWindowScene *PCActiveScene;
static NSMutableArray<UIWindow *> *PCPendingWindows;

@interface UIWindow (PCSceneAttachment)
- (instancetype)pc_initWithFrame:(CGRect)frame;
@end

@implementation UIWindow (PCSceneAttachment)

+ (void)load {
    PCPendingWindows = [[NSMutableArray alloc] init];
    Method original = class_getInstanceMethod(self, @selector(initWithFrame:));
    Method replacement = class_getInstanceMethod(self, @selector(pc_initWithFrame:));
    if (original != NULL && replacement != NULL) {
        method_exchangeImplementations(original, replacement);
    }
}

- (instancetype)pc_initWithFrame:(CGRect)frame {
    UIWindow *window = [self pc_initWithFrame:frame];
    if ([NSStringFromClass(window.class) isEqualToString:@"WinitUIWindow"]) {
        if (PCActiveScene != nil) {
            window.windowScene = PCActiveScene;
        } else {
            [PCPendingWindows addObject:window];
        }
    }
    return window;
}

static void PCAttachWindow(UIWindow *window, UIWindowScene *scene) {
    window.windowScene = scene;
    [window.rootViewController.view setNeedsLayout];
    [window makeKeyAndVisible];
}

@end

// Scene-lifecycle adoption for iOS 26+/27: the OS traps at launch when an app neither adopts
// UIScene nor is pre-scene legacy (UIApplicationEvaluateRuntimeIssueForNoSceneLifecycleAdoption,
// EXC_BREAKPOINT right after launch). Adoption itself is declarative: an empty UIResponder
// conforming to UIWindowSceneDelegate, named in Info.plist's UIApplicationSceneManifest.
//
// The window is driven by winit through the classic path: it creates its UIWindow during
// application-did-finish-launching — possibly BEFORE this scene connects. A window left without
// a scene gets no layout pass and renders nothing (egui's viewport stays its 10000x10000
// default: the black screen). So this delegate attaches every scene-less window to the scene
// connecting now. The window's FRAME is deliberately never set here: at willConnect the scene's
// geometry is a transitional half-initialized value (834x449 was observed on the iPad Pro 11
// 27.0 simulator) and UIKit sizes a scene's windows itself on the next layout pass — setting
// the frame manually cemented the transitional size (content offset into the middle of the
// screen) and stuck, because later retries skip already-attached windows.
//
// NSLog diagnostics (visible via `xcrun simctl spawn <udid> log show --predicate
// 'process == "PhotoCraft"'`): window counts at each hook.
@interface PCSceneDelegate : UIResponder <UIWindowSceneDelegate>
@end

@implementation PCSceneDelegate

- (void)attachWindowsToScene:(UIWindowScene *)scene cause:(NSString *)cause {
    NSUInteger attached = 0;
    PCActiveScene = scene;
    for (UIWindow *window in PCPendingWindows) {
        PCAttachWindow(window, scene);
        attached++;
    }
    [PCPendingWindows removeAllObjects];
    NSLog(@"PCSD %@: scene=%p attachedPending=%lu", cause, scene, (unsigned long)attached);
}

- (void)scene:(UIScene *)scene willConnectToSession:(UISceneSession *)session options:(UISceneConnectionOptions *)connectionOptions {
    NSLog(@"PCSD willConnect begin: scene=%p", scene);
    if (![scene isKindOfClass:[UIWindowScene class]]) {
        NSLog(@"PCSD willConnect: not a window scene, ignoring");
        return;
    }
    [self attachWindowsToScene:(UIWindowScene *)scene cause:@"willConnect"];
    // A window created after this hook (winit's is created inside didFinishLaunching, whose
    // order against scene connection is not guaranteed across OS versions) needs another pass:
    // repeat on the main queue a few times over the first seconds of the run.
    for (NSNumber *delayValue in @[@0.5, @2.0, @5.0]) {
        double delay = delayValue.doubleValue;
        UIWindowScene *captured = (UIWindowScene *)scene;
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(delay * NSEC_PER_SEC)),
                       dispatch_get_main_queue(), ^{
            [self attachWindowsToScene:captured
                                cause:[NSString stringWithFormat:@"willConnect+%.1fs", delay]];
        });
    }
}

// A scene that activates after connecting (return from background, stage manager) may again
// find a window that arrived without one; the same attachment covers it.
- (void)sceneDidBecomeActive:(UIScene *)scene {
    NSLog(@"PCSD sceneDidBecomeActive: scene=%p", scene);
    if (![scene isKindOfClass:[UIWindowScene class]]) {
        return;
    }
    [self attachWindowsToScene:(UIWindowScene *)scene cause:@"didBecomeActive"];
}

@end
