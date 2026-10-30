class Base:
    """Root model."""
    def save(self):
        pass

class User(Base):
    def __init__(self, name: str):
        self.name = name

    def greet(self) -> str:
        return format_name(self.name)

def format_name(n: str) -> str:
    return n.title()
