import Foundation

protocol Greeter {
    func greet(name: String) -> String
}

struct Hello: Greeter {
    let prefix: String

    func greet(name: String) -> String {
        return "\(prefix) \(name)"
    }
}

enum Mode {
    case quiet
    case loud
}
