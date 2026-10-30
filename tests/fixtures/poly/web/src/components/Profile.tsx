import React from "react";
import { HttpClient, Client } from "../lib/api";
import { Avatar } from "./Avatar";

export const Profile = ({ id }: { id: string }) => {
  const client: Client = new HttpClient();
  client.get(`/users/${id}`);
  return <div><Avatar size={3} /></div>;
};
