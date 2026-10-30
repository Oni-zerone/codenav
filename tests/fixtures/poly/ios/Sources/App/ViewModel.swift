import SwiftUI

class ViewModel: ObservableObject {
    let repo: Repository = ItemStore()

    func refresh() {
        let items = repo.load()
        render(items)
    }

    func render(_ items: [Item]) {}
}

extension ViewModel: CustomStringConvertible {
    var description: String { "vm" }
    func reset() { refresh() }
}
