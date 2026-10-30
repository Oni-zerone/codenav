from app.models import User, format_name
from ..models import Base
import requests

class UserService:
    def create(self, name: str) -> User:
        u = User(name)
        u.save()
        self.notify(u)
        return u

    def notify(self, user: User):
        requests.post("/hook", json={"n": format_name(user.name)})
