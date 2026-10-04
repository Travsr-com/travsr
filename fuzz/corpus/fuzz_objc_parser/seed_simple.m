#import <Foundation/Foundation.h>

@protocol Speakable <NSObject>
- (NSString *)speak;
@end

@interface Greeter : NSObject <Speakable>
@property (nonatomic, copy) NSString *prefix;
- (NSString *)greetName:(NSString *)name loudly:(BOOL)loudly;
@end

@implementation Greeter
- (NSString *)speak {
    return self.prefix;
}

- (NSString *)greetName:(NSString *)name loudly:(BOOL)loudly {
    return [NSString stringWithFormat:@"%@ %@", self.prefix, name];
}
@end
