import Foundation

protocol Repository {
    func load() -> [Item]
}

struct Item {
    let title: String
}

final class ItemStore: Repository {
    init() {}
    func load() -> [Item] { return [Item(title: "a")] }
}
